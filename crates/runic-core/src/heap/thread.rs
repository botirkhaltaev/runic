use core::{
    cell::{Cell, UnsafeCell},
    num::NonZeroU32,
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
use super::{AllocatorCtx, Heap};

fn idle_heap(heap: &Heap) -> bool {
    heap.inboxes_empty() && !heap.occupied()
}

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`
    /// with the `PageOwner` `free_slow` already looked up.
    Remote(PageOwner),
    Heap(HeapError),
}

/// Thread-local frontend: owned heaps and per-class current run.
///
/// Hit is current-run pop / `Run::free`. Miss / bind / unbind / adopt take
/// [`AllocatorCtx`]. `lookup` is miss / realloc. The list front is the alloc heap.
pub(crate) struct ThreadHeaps {
    heaps: UnsafeCell<LinkedList<'static, Heap>>,
    current: [Cell<Option<&'static Run>>; SizeClasses::COUNT],
    #[cfg(feature = "c-abi")]
    exit_armed: Cell<bool>,
}

impl ThreadHeaps {
    const fn new() -> Self {
        Self {
            heaps: UnsafeCell::new(LinkedList::new()),
            current: [const { Cell::new(None) }; SizeClasses::COUNT],
            #[cfg(feature = "c-abi")]
            exit_armed: Cell::new(false),
        }
    }

    #[cfg(not(feature = "c-abi"))]
    fn arm_exit() {
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

    fn captured_id(heap: &Heap) -> HeapId {
        let Some(generation) = NonZeroU32::new(heap.thread_gen.get()) else {
            Allocator::abort();
        };
        HeapId::from_slot(heap.slot, generation)
    }

    fn link_front(&self, heap: &'static Heap) {
        heap.thread_gen.set(heap.id().generation().get());
        self.with_heaps_mut(|heaps| heaps.push_front(heap));
    }

    fn link_back(&self, heap: &'static Heap) {
        heap.thread_gen.set(heap.id().generation().get());
        self.with_heaps_mut(|heaps| heaps.push_back(heap));
    }

    fn owns(&self, owner: &Heap) -> bool {
        let id = owner.id();
        self.heaps()
            .iter()
            .any(|heap| heap == owner && Self::captured_id(heap) == id)
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

    /// Freelist empty: `extend`, then a run an attached heap already holds, then
    /// `acquire` on the front heap.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        class: SizeClass,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(front) = self.heap() else {
            return Ok(None);
        };
        if let Some(ptr) = self.extend_current(class) {
            return Ok(Some(ptr));
        }
        let Some(run) = self.take_run(front, class, ctx)? else {
            return Ok(None);
        };
        self.set_current(class, Some(run));
        Ok(self.extend_current(class))
    }

    /// Flush each attached heap and take a run for `class`. Adopted heaps only
    /// hand over a run they already hold, which drains them toward idle. The
    /// front heap `acquire`s: its own list, then a new run. One guard at a time.
    fn take_run(
        &self,
        front: &'static Heap,
        class: SizeClass,
        ctx: &AllocatorCtx,
    ) -> Result<Option<&'static Run>, HeapError> {
        for heap in self.heaps().iter().skip(1) {
            heap.flush_owner(ctx)?;
            if let Some(run) = heap.require_inner().runs.take_available(class) {
                return Ok(Some(run));
            }
        }
        front.flush_owner(ctx)?;
        Ok(front.require_inner().runs.acquire(class, front, ctx.pages))
    }

    fn extend_current(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = self.current(class)?;
        run.allocate().or_else(|| {
            run.extend();
            run.allocate()
        })
    }

    /// Owner-local large allocation. Cached extents on any attached heap, then a
    /// new mapping on the front heap.
    ///
    /// Returns `None` when this thread has no attached heap (caller should `bind`).
    #[inline(never)]
    pub(crate) fn alloc_extent(
        &self,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(front) = self.heap() else {
            return Ok(None);
        };
        // Same order as `take_run`: adopted heaps only reuse; the front heap
        // reuses, then maps.
        for heap in self.heaps().iter().skip(1) {
            heap.flush_owner(ctx)?;
            if let Some(ptr) = heap
                .require_inner()
                .extents
                .reuse_cached(spec, heap, ctx.pages, init)?
            {
                return Ok(Some(ptr));
            }
        }
        front.flush_owner(ctx)?;
        front
            .require_inner()
            .extents
            .allocate(spec, front, ctx.pages, init)
    }

    /// Owner-local free for a run owned by a TLS heap.
    ///
    /// `Run::free` is lock-free. `RunHeap::release` runs when the run left full
    /// or its payload is discardable. Heap-id stays: lookup can still return a
    /// foreign run.
    pub(crate) fn free_run(
        &self,
        run: &'static Run,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        if !self.owns(run.heap()) {
            return Err(ThreadFreeError::Remote(PageOwner::Run(run)));
        }
        let Ok(outcome) = run.free(ptr) else {
            Allocator::abort()
        };
        if outcome == RunFree::Available || run.is_discardable() {
            let mut inner = run.heap().require_inner();
            if inner.release(run, outcome).is_err() {
                Allocator::abort();
            }
        }
        Ok(())
    }

    /// Owner-local free for an extent owned by a TLS heap.
    pub(crate) fn free_extent(
        &self,
        extent: &'static Extent,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx,
    ) -> Result<(), ThreadFreeError> {
        if !self.owns(extent.heap()) {
            return Err(ThreadFreeError::Remote(PageOwner::Extent(extent)));
        }

        let mut inner = extent.heap().require_inner();
        inner
            .free(PageOwner::Extent(extent), ptr, ctx.pages)
            .map(|_| ())
            .map_err(ThreadFreeError::Heap)
    }

    /// Bind this thread to a heap in `ctx`.
    ///
    /// Reuses the alloc heap already at the front of the list; otherwise
    /// acquires one and links it there.
    #[cold]
    pub(crate) fn bind(&self, ctx: &AllocatorCtx<'static>) -> Option<HeapId> {
        #[cfg(not(feature = "c-abi"))]
        Self::arm_exit();
        #[cfg(feature = "c-abi")]
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
        #[cfg(not(feature = "c-abi"))]
        Self::arm_exit();
        #[cfg(feature = "c-abi")]
        self.arm_exit();
        if self.owns(heap) {
            return true;
        }
        if heap.adopt(heap.id()).is_err() {
            return false;
        }
        self.link_back(heap);
        if heap.flush_owner(ctx).is_err() {
            Allocator::abort();
        }
        true
    }

    fn unbind_heap(&self, heap: &'static Heap, ctx: &AllocatorCtx) {
        let id = Self::captured_id(heap);
        heap.thread_gen.set(0);
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
            loop {
                let remove = cursor
                    .current()
                    .is_some_and(|heap| heap == owner && idle_heap(heap));
                if remove {
                    return cursor.remove_current();
                }
                cursor.current()?;
                cursor.move_next();
            }
        })
    }

    /// Owner-local free. Unbind the owner if it is idle.
    pub(crate) fn free_owner(
        &self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx,
    ) -> Result<(), ThreadFreeError> {
        match owner {
            PageOwner::Run(run) => {
                self.free_run(run, ptr)?;
                if !run.is_live()
                    && let Some(heap) = self.take_idle(run.heap())
                {
                    self.unbind_heap(heap, ctx);
                }
                Ok(())
            }
            PageOwner::Extent(extent) => {
                self.free_extent(extent, ptr, ctx)?;
                if let Some(heap) = self.take_idle(extent.heap()) {
                    self.unbind_heap(heap, ctx);
                }
                Ok(())
            }
        }
    }

    /// Miss / large / unbound: `lookup` then [`Self::free_owner`].
    #[inline(never)]
    pub(crate) fn free_slow(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
        ctx: &AllocatorCtx,
    ) -> Result<(), ThreadFreeError> {
        let Some(owner) = Allocator::lookup(ctx.pages, ptr, spec) else {
            let error = if SizeClasses::class_for(spec).is_some() {
                HeapError::InvalidRunPointer
            } else {
                HeapError::MissingExtent
            };
            return Err(ThreadFreeError::Heap(error));
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

    fn heap(&self) -> Option<&'static Heap> {
        self.heaps().front()
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
                if guard.push_available(run).is_err() {
                    Allocator::abort();
                }
            }
            cell.set(None);
        }
    }

    /// Unbind TLS heaps. `live` stays exact. The process payload stays.
    ///
    /// Non-full current runs go back on the available list so reincarnation
    /// can reuse them. `push_available` is idempotent if a run is already linked.
    #[cold]
    pub(crate) fn unbind(&self, ctx: &AllocatorCtx) {
        while let Some(heap) = self.with_heaps_mut(LinkedList::pop_front) {
            self.unbind_heap(heap, ctx);
        }
    }

    fn exit(&self) {
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
