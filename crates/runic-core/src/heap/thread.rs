use core::{
    cell::{Cell, UnsafeCell},
    ptr::NonNull,
};

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

/// Open remote chains, grouped as eight sets of two. A run hashes into its
/// set, so a burst hits one of two compares. A chain stays open until its
/// set needs the slot, or the thread is holding [`REMOTE_BUDGET`] bytes.
const REMOTE_SETS: usize = 8;
const REMOTE_WAYS: usize = 2;
const REMOTE_SLOTS: usize = REMOTE_SETS * REMOTE_WAYS;
/// Claimed bytes a thread holds before every open chain is pushed.
const REMOTE_BUDGET: usize = 16 * 1024;
/// Odd mix. Run bases are 64 KiB aligned, so `>> 16` keeps the address bits that differ.
const REMOTE_MIX: usize = 0x7EFB_352D;

const _: () = assert!(REMOTE_SETS.is_power_of_two());

/// One open chain of claimed blocks for `run`. Empty when `run` is `None`.
struct RemoteSlot {
    run: Cell<Option<&'static Run>>,
    head: Cell<Option<NonNull<u8>>>,
    tail: Cell<Option<NonNull<u8>>>,
    bytes: Cell<usize>,
}

/// A chain taken off its slot. The freer owns it until [`Run::push`].
struct Chain {
    run: &'static Run,
    head: NonNull<u8>,
    tail: NonNull<u8>,
    bytes: usize,
}

impl RemoteSlot {
    const fn new() -> Self {
        Self {
            run: Cell::new(None),
            head: Cell::new(None),
            tail: Cell::new(None),
            bytes: Cell::new(0),
        }
    }

    /// Link `ptr` at the head. The slot is empty or already holds `run`.
    /// Returns the block size and whether this opened the chain.
    fn link(&self, run: &'static Run, ptr: NonNull<u8>) -> (usize, bool) {
        let size = run.class().size();
        let previous = self.head.replace(Some(ptr));
        Freelist::link(ptr, previous);
        let opened = previous.is_none();
        if opened {
            self.run.set(Some(run));
            self.tail.set(Some(ptr));
            self.bytes.set(size);
        } else {
            self.bytes.set(self.bytes.get() + size);
        }
        (size, opened)
    }

    /// Take the open chain. `None` when the slot is empty.
    fn take(&self) -> Option<Chain> {
        let run = self.run.take()?;
        let (Some(head), Some(tail)) = (self.head.take(), self.tail.take()) else {
            Allocator::abort();
        };
        Some(Chain {
            run,
            head,
            tail,
            bytes: self.bytes.replace(0),
        })
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
    /// Claimed bytes sitting in `remote`. Pushed at [`REMOTE_BUDGET`].
    remote_bytes: Cell<usize>,
    exit: ExitHook,
}

impl ThreadHeaps {
    const fn new() -> Self {
        Self {
            heaps: UnsafeCell::new(LinkedList::new()),
            current: [const { Cell::new(None) }; SizeClasses::COUNT],
            remote: [const { RemoteSlot::new() }; REMOTE_SLOTS],
            remote_bytes: Cell::new(0),
            exit: ExitHook::new(),
        }
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
        self.exit.arm();
        heap.thread_id.set(Some(heap.id()));
        self.with_heaps_mut(|heaps| heaps.push_front(heap));
    }

    fn link_back(&self, heap: &'static Heap) {
        self.exit.arm();
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
    /// The run hashes into a set of two slots. A hit is one link. A miss pushes
    /// the fullest slot in that set, then opens `run` there. The thread pushes
    /// every open chain once [`REMOTE_BUDGET`] bytes are claimed. A Draining
    /// retry lives in `enqueue_remote` and does not push the chain again.
    pub(crate) fn hold(&self, run: &'static Run, ptr: NonNull<u8>) -> Result<(), HeapError> {
        run.claim(ptr)?;
        let base = Self::set_of(run) * REMOTE_WAYS;
        for way in 0..REMOTE_WAYS {
            let slot = self.slot(base + way);
            if slot.run.get() == Some(run) {
                let (bytes, opened) = slot.link(run, ptr);
                self.linked(bytes, opened);
                return self.push_over_budget();
            }
        }
        let slot = self.fullest(base);
        if slot.run.get().is_some() {
            self.push_slot(slot)?;
        }
        let (bytes, opened) = slot.link(run, ptr);
        self.linked(bytes, opened);
        self.push_over_budget()
    }

    fn set_of(run: &Run) -> usize {
        let key = core::ptr::from_ref(run).addr();
        (key.wrapping_mul(REMOTE_MIX) >> 16) & (REMOTE_SETS - 1)
    }

    fn slot(&self, index: usize) -> &RemoteSlot {
        self.remote.get(index).unwrap_or_else(|| Allocator::abort())
    }

    /// An empty slot in the set, or the one holding the most bytes.
    fn fullest(&self, base: usize) -> &RemoteSlot {
        let mut choice = self.slot(base);
        let mut most = 0usize;
        for way in 0..REMOTE_WAYS {
            let slot = self.slot(base + way);
            if slot.run.get().is_none() {
                return slot;
            }
            let bytes = slot.bytes.get();
            if bytes > most {
                most = bytes;
                choice = slot;
            }
        }
        choice
    }

    /// Count a linked block. The first block of a chain arms thread exit:
    /// a thread that only frees never links a heap.
    fn linked(&self, bytes: usize, opened: bool) {
        if opened {
            self.exit.arm();
        }
        self.remote_bytes.set(self.remote_bytes.get() + bytes);
    }

    fn push_over_budget(&self) -> Result<(), HeapError> {
        if self.remote_bytes.get() < REMOTE_BUDGET {
            return Ok(());
        }
        self.push_remote()
    }

    /// Push every open chain. A Draining free adopts next; claimed blocks still
    /// in a slot would keep that heap live and linked for the rest of the process.
    #[cold]
    pub(crate) fn push_remote(&self) -> Result<(), HeapError> {
        for slot in &self.remote {
            self.push_slot(slot)?;
        }
        Ok(())
    }

    #[cold]
    fn push_slot(&self, slot: &RemoteSlot) -> Result<(), HeapError> {
        let Some(chain) = slot.take() else {
            return Ok(());
        };
        chain.run.push(chain.head, chain.tail);
        let Some(left) = self.remote_bytes.get().checked_sub(chain.bytes) else {
            Allocator::abort();
        };
        self.remote_bytes.set(left);
        let Some(ctx) = Allocator::ctx() else {
            Allocator::abort();
        };
        let owner = chain.run.heap();
        let id = owner.active_id().unwrap_or_else(|| owner.id());
        Allocator::enqueue_remote(&ctx, owner, id, PageOwner::Run(chain.run))
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

    /// Thread exit. The hook is armed only once state landed on this thread,
    /// and state comes from an initialized allocator, so a missing `ctx` is a
    /// broken invariant.
    fn exit(&self) {
        let Some(ctx) = Allocator::ctx() else {
            Allocator::abort();
        };
        self.unbind(&ctx);
    }
}

/// Runs [`ThreadHeaps::exit`] when the thread ends.
///
/// Armed once per thread, the first time state lands on `THREAD_HEAPS`: a
/// heap is linked or a remote chain opens. The registration leaf is the only
/// build-dependent part.
struct ExitHook {
    armed: Cell<bool>,
}

impl ExitHook {
    const fn new() -> Self {
        Self {
            armed: Cell::new(false),
        }
    }

    fn arm(&self) {
        if !self.armed.replace(true) {
            Self::register();
        }
    }

    /// A `thread_local!` guard whose drop is the exit.
    #[cfg(not(feature = "c-abi"))]
    fn register() {
        struct Guard;

        impl Drop for Guard {
            fn drop(&mut self) {
                THREAD_HEAPS.exit();
            }
        }

        std::thread_local! {
            static GUARD: Guard = const { Guard };
        }

        GUARD.with(|_| {});
    }

    /// A `pthread` key, not `thread_local!`: glibc registers Rust TLS
    /// destructors through `__cxa_thread_atexit_impl`, which allocates. When
    /// Runic is the process allocator (`LD_PRELOAD`), that allocation re-enters
    /// [`ThreadHeaps::bind`] and recurses until the stack is gone.
    #[cfg(feature = "c-abi")]
    fn register() {
        static KEY: spin::Once<libc::pthread_key_t> = spin::Once::new();

        extern "C" fn exit(_: *mut core::ffi::c_void) {
            THREAD_HEAPS.exit();
        }

        let key = *KEY.call_once(|| {
            let mut key = 0;
            // SAFETY: `key` is a live out-parameter and `exit` lives for the process.
            if unsafe { libc::pthread_key_create(&raw mut key, Some(exit)) } != 0 {
                Allocator::abort();
            }
            key
        });
        // SAFETY: `key` came from `pthread_key_create`. The value only has to be
        // non-null for the callback to run, and storing it does not allocate.
        if unsafe { libc::pthread_setspecific(key, core::ptr::without_provenance_mut(1)) } != 0 {
            Allocator::abort();
        }
    }
}

#[thread_local]
pub(crate) static THREAD_HEAPS: ThreadHeaps = ThreadHeaps::new();
