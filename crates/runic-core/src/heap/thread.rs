use core::{
    cell::{Cell, UnsafeCell},
    ptr::NonNull,
};

#[cfg(feature = "c-abi")]
use core::ffi::c_void;
#[cfg(feature = "c-abi")]
use spin::Once;

use crate::{
    allocator::Allocator,
    heap::{Extent, ExtentInit, HeapError, HeapId, Run, RunError, RunFree},
    layout::LayoutSpec,
    memory::PageOwner,
    size_class::{SizeClass, SizeClasses},
};

use super::list::LinkedList;
use super::run::Freelist;
use super::{AllocatorCtx, Heap};

/// Open remote chains. The front slot is the run most recently freed, so a
/// burst hits one compare. Eight covers the runs a freer of a worker pool
/// actually touches; a full front slot, or the fullest slot when all are in
/// use, is what pushes.
const REMOTE_SLOTS: usize = 8;
/// Blocks per chain before `Run::push`. One push replaces per-block inbox traffic.
const CHAIN_LIMIT: u16 = 16;

/// One open chain of claimed blocks for `run`. Empty when `run` is `None`.
struct RemoteSlot {
    run: Cell<Option<&'static Run>>,
    head: Cell<Option<NonNull<u8>>>,
    tail: Cell<Option<NonNull<u8>>>,
    count: Cell<u16>,
}

impl RemoteSlot {
    const fn new() -> Self {
        Self {
            run: Cell::new(None),
            head: Cell::new(None),
            tail: Cell::new(None),
            count: Cell::new(0),
        }
    }
}

/// Thread-local frontend: owned heaps and per-class current run.
///
/// Hit is current-run pop / `Run::free`. Miss / bind / unbind / adopt take
/// [`AllocatorCtx`]. `lookup` is miss / realloc. Bind links at the front.
/// Adopt links at the back. Allocation walks every attached heap.
pub(crate) struct ThreadHeaps {
    heaps: UnsafeCell<LinkedList<'static, Heap>>,
    current: [Cell<Option<&'static Run>>; SizeClasses::COUNT],
    remote: [RemoteSlot; REMOTE_SLOTS],
    #[cfg(feature = "c-abi")]
    exit_armed: Cell<bool>,
}

impl ThreadHeaps {
    const fn new() -> Self {
        Self {
            heaps: UnsafeCell::new(LinkedList::new()),
            current: [const { Cell::new(None) }; SizeClasses::COUNT],
            remote: [const { RemoteSlot::new() }; REMOTE_SLOTS],
            #[cfg(feature = "c-abi")]
            exit_armed: Cell::new(false),
        }
    }

    /// Arm this thread's exit callback. Idempotent; called from every cold
    /// entry that leaves state on this thread: `bind`, `adopt`, and the first
    /// remote hold of a run.
    #[cfg(not(feature = "c-abi"))]
    fn arm_exit(&self) {
        UNBIND_GUARD.with(|_| {});
    }

    #[cfg(feature = "c-abi")]
    fn arm_exit(&self) {
        if self.exit_armed.get() {
            return;
        }
        UNBIND_HOOK.arm();
        self.exit_armed.set(true);
    }

    fn heaps(&self) -> &LinkedList<'static, Heap> {
        // SAFETY: `THREAD_HEAPS` is thread-local. Callers do not overlap list borrows.
        unsafe { &*self.heaps.get() }
    }

    fn with_heaps_mut<R>(&self, f: impl FnOnce(&mut LinkedList<'static, Heap>) -> R) -> R {
        // SAFETY: `THREAD_HEAPS` is thread-local. The closure ends the mutable borrow
        // before another borrow of the list.
        f(unsafe { &mut *self.heaps.get() })
    }

    /// The id captured when `heap` was linked. Unlinked heaps abort.
    fn captured_id(heap: &Heap) -> HeapId {
        heap.thread_id.get().unwrap_or_else(|| Allocator::abort())
    }

    fn link_front(&self, heap: &'static Heap) {
        heap.thread_id.set(Some(heap.id()));
        self.with_heaps_mut(|heaps| heaps.push_front(heap));
    }

    fn link_back(&self, heap: &'static Heap) {
        heap.thread_id.set(Some(heap.id()));
        self.with_heaps_mut(|heaps| heaps.push_back(heap));
    }

    pub(crate) fn owns(&self, owner: &Heap) -> bool {
        !self.is_remote(owner)
    }

    /// `owner` is not an attached heap of this thread.
    ///
    /// The generation check runs only when the pointer matches, so a remote
    /// free walks pointers and returns.
    pub(crate) fn is_remote(&self, owner: &Heap) -> bool {
        for heap in self.heaps().iter() {
            if heap == owner {
                return Self::captured_id(heap) != owner.id();
            }
        }
        true
    }

    /// Owner-local small allocation via the current run for `class`.
    ///
    /// Hit is pop. Empty / unbound → caller miss.
    #[inline]
    pub(crate) fn alloc(&self, class: SizeClass) -> Option<NonNull<u8>> {
        self.current(class)?.allocate()
    }

    /// Owner-local small free via the current run for `class`.
    ///
    /// Hit is `Run::free` (`locate` + push). Ignores the `RunFree` outcome and Discard.
    /// `OutOfRange` / unbound → caller `dealloc_slow`. Interior is
    /// `InvalidPointer` → abort.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>, class: SizeClass) -> Option<()> {
        match self.current(class)?.free(ptr) {
            Ok(_) => Some(()),
            Err(RunError::OutOfRange) => None,
            Err(_) => Allocator::abort(),
        }
    }

    /// Freelist empty: `extend`, then a run any attached heap already holds,
    /// then `acquire` on the first heap that can map one.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        class: SizeClass,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        if self.heaps().is_empty() {
            return Ok(None);
        }
        if let Some(ptr) = self.extend_current(class) {
            return Ok(Some(ptr));
        }
        let Some(run) = self.take_run(class, ctx)? else {
            return Ok(None);
        };
        self.set_current(class, Some(run));
        Ok(self.extend_current(class))
    }

    /// Every attached heap is the same. Flush and take a run it already holds.
    /// If none has one, map on the first heap whose `acquire` succeeds.
    /// One guard at a time.
    fn take_run(
        &self,
        class: SizeClass,
        ctx: &AllocatorCtx,
    ) -> Result<Option<&'static Run>, HeapError> {
        for heap in self.heaps().iter() {
            heap.flush_owner(ctx)?;
            if let Some(run) = heap.require_inner().runs.take_available(class) {
                return Ok(Some(run));
            }
        }
        for heap in self.heaps().iter() {
            if let Some(run) = heap.require_inner().runs.acquire(class, heap, ctx.pages) {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    fn extend_current(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = self.current(class)?;
        run.allocate().or_else(|| {
            run.extend();
            run.allocate()
        })
    }

    /// Owner-local large allocation. Cached extents on any attached heap, then
    /// a new mapping on the first heap that can map one.
    ///
    /// Returns `None` when this thread has no attached heap (caller should `bind`).
    #[inline(never)]
    pub(crate) fn alloc_extent(
        &self,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        if self.heaps().is_empty() {
            return Ok(None);
        }
        for heap in self.heaps().iter() {
            heap.flush_owner(ctx)?;
            if let Some(ptr) = heap
                .require_inner()
                .extents
                .reuse_cached(spec, heap, ctx.pages, init)?
            {
                return Ok(Some(ptr));
            }
        }
        for heap in self.heaps().iter() {
            if let Some(ptr) = heap
                .require_inner()
                .extents
                .allocate(spec, heap, ctx.pages, init)?
            {
                return Ok(Some(ptr));
            }
        }
        Ok(None)
    }

    /// Owner-local free for a run this thread owns.
    ///
    /// `Run::free` is lock-free. `RunHeap::release` runs when the run left full
    /// or its payload is discardable. A remote run belongs to
    /// [`Self::free_owner`]; calling this on one aborts.
    pub(crate) fn free_run(&self, run: &'static Run, ptr: NonNull<u8>) -> Result<(), HeapError> {
        if !self.owns(run.heap()) {
            Allocator::abort();
        }
        let outcome = run.free(ptr)?;
        if outcome == RunFree::Available || run.is_discardable() {
            let mut inner = run.heap().require_inner();
            inner.runs.release(run, outcome)?;
        }
        Ok(())
    }

    /// Owner-local free for an extent this thread owns.
    ///
    /// A remote extent belongs to [`Self::free_owner`]; calling this on one aborts.
    pub(crate) fn free_extent(
        &self,
        extent: &'static Extent,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx,
    ) -> Result<(), HeapError> {
        if !self.owns(extent.heap()) {
            Allocator::abort();
        }
        let mut inner = extent.heap().require_inner();
        inner
            .free(PageOwner::Extent(extent), ptr, ctx.pages)
            .map(|_| ())
    }

    /// Bind this thread to a heap in `ctx`.
    ///
    /// Already attached: return the front heap. Otherwise acquire one and link
    /// it at the front.
    #[cold]
    pub(crate) fn bind(&self, ctx: &AllocatorCtx<'static>) -> Option<HeapId> {
        self.arm_exit();
        if let Some(heap) = self.heaps().front() {
            return Some(Self::captured_id(heap));
        }

        let heap = ctx.heaps.acquire()?;
        self.link_front(heap);
        Some(Self::captured_id(heap))
    }

    /// First Draining freer becomes Active owner and stays on this thread's list.
    #[cold]
    pub(crate) fn adopt(&self, heap: &'static Heap, ctx: &AllocatorCtx) -> bool {
        self.arm_exit();
        if self.owns(heap) {
            return true;
        }
        let Ok(mut inner) = heap.adopt(heap.id()) else {
            return false;
        };
        self.link_back(heap);
        // The adopt guard covers this first flush, so no `require_inner` runs
        // while a Draining admit may still be checking the state under the lock.
        if heap.flush(&mut inner, ctx, None).is_err() {
            Allocator::abort();
        }
        true
    }

    fn unbind_heap(&self, heap: &'static Heap, ctx: &AllocatorCtx) {
        let id = Self::captured_id(heap);
        heap.thread_id.set(None);
        self.release_current(heap);
        if ctx.heaps.unbind(id, ctx).is_err() {
            Allocator::abort();
        }
    }

    /// Unlink an idle heap when another heap is still attached, so the last
    /// heap keeps its retained empty runs.
    fn take_idle(&self, owner: &Heap) -> Option<&'static Heap> {
        self.with_heaps_mut(|heaps| {
            if heaps.len() <= 1 {
                return None;
            }
            let mut cursor = heaps.cursor_front_mut();
            while let Some(heap) = cursor.current() {
                if heap == owner && heap.is_idle() {
                    return cursor.remove_current();
                }
                cursor.move_next();
            }
            None
        })
    }

    /// Free `owner` on this thread when it owns the heap, and unbind that heap
    /// when it is idle. Any other heap goes through [`Allocator::free_remote`].
    pub(crate) fn free_owner(
        &self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx,
    ) -> Result<(), HeapError> {
        if !self.owns(owner.heap()) {
            return Allocator::free_remote(ctx, owner, ptr);
        }
        match owner {
            PageOwner::Run(run) => {
                self.free_run(run, ptr)?;
                if !run.is_live()
                    && let Some(heap) = self.take_idle(run.heap())
                {
                    self.unbind_heap(heap, ctx);
                }
            }
            PageOwner::Extent(extent) => {
                self.free_extent(extent, ptr, ctx)?;
                if let Some(heap) = self.take_idle(extent.heap()) {
                    self.unbind_heap(heap, ctx);
                }
            }
        }
        Ok(())
    }

    /// Miss / large / unbound: `lookup` then [`Self::free_owner`].
    #[inline(never)]
    pub(crate) fn free_slow(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
        ctx: &AllocatorCtx,
    ) -> Result<(), HeapError> {
        let Some(owner) = Allocator::lookup(ctx.pages, ptr, spec) else {
            return Err(if SizeClasses::class_for(spec).is_some() {
                HeapError::InvalidRunPointer
            } else {
                HeapError::MissingExtent
            });
        };
        self.free_owner(owner, ptr, ctx)
    }

    fn current(&self, class: SizeClass) -> Option<&Run> {
        debug_assert!(class.index() < self.current.len());
        // SAFETY: trusted constructor. `SizeClass` index is `< SizeClasses::COUNT`, and `current.len()` is that count.
        let cell = unsafe { self.current.get_unchecked(class.index()) };
        cell.get()
    }

    fn set_current(&self, class: SizeClass, run: Option<&'static Run>) {
        debug_assert!(class.index() < self.current.len());
        // SAFETY: trusted constructor. `SizeClass` index is `< SizeClasses::COUNT`, and `current.len()` is that count.
        unsafe { self.current.get_unchecked(class.index()) }.set(run);
    }

    /// Push this heap's non-full current runs back onto it and clear those cells.
    fn release_current(&self, owner: &Heap) {
        let mut inner = None;
        for cell in &self.current {
            let Some(run) = cell.get() else {
                continue;
            };
            if run.heap() != owner {
                continue;
            }
            if !run.is_full() {
                let guard = inner.get_or_insert_with(|| owner.require_inner());
                if guard.runs.push_available(run).is_err() {
                    Allocator::abort();
                }
            }
            cell.set(None);
        }
    }

    /// Claim `ptr` and link it on this thread's chain for `run`.
    ///
    /// The front slot is that run after the first block of a burst. A full
    /// chain, or the fullest chain when every slot is in use, is [`Run::push`]
    /// then [`Allocator::enqueue_remote`]. A Draining retry lives in
    /// `enqueue_remote` and does not push the chain again.
    pub(crate) fn hold(&self, run: &'static Run, ptr: NonNull<u8>) -> Result<(), HeapError> {
        run.claim(ptr)?;
        let Some(slot) = self.remote.first() else {
            Allocator::abort();
        };
        if slot.run.get() != Some(run) {
            self.bring_front(run)?;
        }
        let head = slot.head.replace(Some(ptr));
        Freelist::link(ptr, head);
        if head.is_none() {
            slot.run.set(Some(run));
            slot.tail.set(Some(ptr));
            slot.count.set(1);
            return Ok(());
        }
        let count = slot.count.get() + 1;
        slot.count.set(count);
        if count == CHAIN_LIMIT {
            self.push_slot(0)?;
        }
        Ok(())
    }

    /// Make slot 0 the chain for `run`: promote a hit, or park an empty slot
    /// there, or flush the fullest chain and park that slot there.
    ///
    /// A thread that only frees never binds, so this is where its exit
    /// callback gets armed; otherwise chains left on its slots would be lost.
    #[cold]
    #[inline(never)]
    fn bring_front(&self, run: &'static Run) -> Result<(), HeapError> {
        self.arm_exit();
        let mut empty = None;
        let mut fullest = 0usize;
        let mut fullest_count = 0u16;
        for (index, slot) in self.remote.iter().enumerate() {
            match slot.run.get() {
                Some(held) if held == run => {
                    self.swap_slots(0, index);
                    return Ok(());
                }
                None => empty = empty.or(Some(index)),
                Some(_) => {
                    let count = slot.count.get();
                    if count >= fullest_count {
                        fullest = index;
                        fullest_count = count;
                    }
                }
            }
        }
        if let Some(index) = empty {
            self.swap_slots(0, index);
            return Ok(());
        }
        self.push_slot(fullest)?;
        self.swap_slots(0, fullest);
        Ok(())
    }

    fn swap_slots(&self, left: usize, right: usize) {
        if left == right {
            return;
        }
        let Some(a) = self.remote.get(left) else {
            Allocator::abort();
        };
        let Some(b) = self.remote.get(right) else {
            Allocator::abort();
        };
        a.run.swap(&b.run);
        a.head.swap(&b.head);
        a.tail.swap(&b.tail);
        a.count.swap(&b.count);
    }

    /// Push every open chain. A Draining free adopts next; claimed blocks still
    /// in a slot would keep that heap live and linked for the rest of the process.
    pub(crate) fn push_remote(&self) -> Result<(), HeapError> {
        for index in 0..REMOTE_SLOTS {
            self.push_slot(index)?;
        }
        Ok(())
    }

    fn push_slot(&self, index: usize) -> Result<(), HeapError> {
        let Some(slot) = self.remote.get(index) else {
            Allocator::abort();
        };
        let Some(run) = slot.run.take() else {
            return Ok(());
        };
        let (Some(head), Some(tail)) = (slot.head.take(), slot.tail.take()) else {
            Allocator::abort();
        };
        slot.count.set(0);
        run.push(head, tail);
        let Some(ctx) = Allocator::ctx() else {
            Allocator::abort();
        };
        let owner = run.heap();
        let id = owner.active_id().unwrap_or_else(|| owner.id());
        Allocator::enqueue_remote(&ctx, owner, id, PageOwner::Run(run))
    }

    /// Unbind TLS heaps. `live` stays exact. The process payload stays.
    ///
    /// Open remote chains are pushed first. Non-full current runs go back on
    /// the available list so reincarnation can reuse them. `push_available` is
    /// idempotent if a run is already linked.
    #[cold]
    pub(crate) fn unbind(&self, ctx: &AllocatorCtx) {
        if self.push_remote().is_err() {
            Allocator::abort();
        }
        while let Some(heap) = self.with_heaps_mut(LinkedList::pop_front) {
            self.unbind_heap(heap, ctx);
        }
    }

    fn exit(&self) {
        if self.push_remote().is_err() {
            Allocator::abort();
        }
        let Some(ctx) = Allocator::ctx() else {
            if !self.heaps().is_empty() {
                Allocator::abort();
            }
            return;
        };
        self.unbind(&ctx);
    }
}

/// Owns the thread-exit callback that unbinds [`THREAD_HEAPS`].
///
/// A `pthread` key, not `std::thread_local!`: glibc registers Rust TLS
/// destructors through `__cxa_thread_atexit_impl`, which allocates. When Runic
/// is the process allocator (`LD_PRELOAD`), that allocation re-enters
/// [`ThreadHeaps::bind`] and recurses until the stack is gone.
#[cfg(feature = "c-abi")]
struct UnbindHook {
    key: Once<libc::pthread_key_t>,
}

#[cfg(feature = "c-abi")]
impl UnbindHook {
    const fn new() -> Self {
        Self { key: Once::new() }
    }

    /// Arm this thread's callback. Idempotent; called from `bind` / `adopt`.
    fn arm(&self) {
        let key = *self.key.call_once(Self::create);
        // SAFETY: `key` came from `pthread_key_create`. The value only has to be
        // non-null for the callback to run, and storing it does not allocate.
        if unsafe { libc::pthread_setspecific(key, core::ptr::without_provenance_mut(1)) } != 0 {
            Allocator::abort();
        }
    }

    fn create() -> libc::pthread_key_t {
        let mut key = 0;
        // SAFETY: `key` is a live out-parameter and `unbind` lives for the process.
        if unsafe { libc::pthread_key_create(&raw mut key, Some(Self::unbind)) } != 0 {
            Allocator::abort();
        }
        key
    }

    extern "C" fn unbind(_: *mut c_void) {
        THREAD_HEAPS.exit();
    }
}

#[thread_local]
pub(crate) static THREAD_HEAPS: ThreadHeaps = ThreadHeaps::new();

#[cfg(feature = "c-abi")]
static UNBIND_HOOK: UnbindHook = UnbindHook::new();

/// Rust-mode thread-exit guard. C interposition cannot use this because glibc
/// allocates while registering its destructor.
#[cfg(not(feature = "c-abi"))]
struct UnbindGuard;

#[cfg(not(feature = "c-abi"))]
impl Drop for UnbindGuard {
    fn drop(&mut self) {
        THREAD_HEAPS.exit();
    }
}

#[cfg(not(feature = "c-abi"))]
std::thread_local! {
    static UNBIND_GUARD: UnbindGuard = const { UnbindGuard };
}
