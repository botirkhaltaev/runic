use core::{cell::Cell, ptr::NonNull};

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

use super::{AllocatorCtx, Heap};

/// One process [`Heap`] this thread owns. `Active` captures generation at bind/adopt.
#[derive(Clone, Copy)]
enum ThreadHeap {
    Vacant,
    Active { heap: &'static Heap, id: HeapId },
}

impl ThreadHeap {
    fn active(heap: &'static Heap) -> Self {
        Self::Active {
            heap,
            id: heap.id(),
        }
    }

    fn heap(self) -> Option<&'static Heap> {
        match self {
            Self::Vacant => None,
            Self::Active { heap, .. } => Some(heap),
        }
    }

    fn id(self) -> Option<HeapId> {
        match self {
            Self::Vacant => None,
            Self::Active { id, .. } => Some(id),
        }
    }

    fn owns(self, owner: &Heap) -> bool {
        match self {
            Self::Vacant => false,
            Self::Active { heap, id } => heap == owner && id == owner.id(),
        }
    }

    fn is_idle(self) -> bool {
        let Some(heap) = self.heap() else {
            return false;
        };
        if !heap.inboxes_empty() || heap.occupied() {
            return false;
        }
        let Some(inner) = heap.try_inner() else {
            return false;
        };
        !inner.has_live()
    }
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
/// [`AllocatorCtx`]. `lookup` is miss / realloc. Alloc uses any active heap.
pub(crate) struct ThreadHeaps {
    heaps: [Cell<ThreadHeap>; 2],
    current: [Cell<Option<&'static Run>>; SizeClasses::COUNT],
    #[cfg(feature = "c-abi")]
    exit_armed: Cell<bool>,
}

impl ThreadHeaps {
    const fn new() -> Self {
        Self {
            heaps: [const { Cell::new(ThreadHeap::Vacant) }; 2],
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

    fn owns(&self, owner: &Heap) -> bool {
        let id = owner.id();
        self.heaps.iter().any(|slot| match slot.get() {
            ThreadHeap::Vacant => false,
            ThreadHeap::Active { heap, id: bound } => heap == owner && bound == id,
        })
    }

    fn vacant(&self) -> Option<&Cell<ThreadHeap>> {
        self.heaps
            .iter()
            .find(|slot| matches!(slot.get(), ThreadHeap::Vacant))
    }

    fn active(&self) -> Option<ThreadHeap> {
        self.heaps.iter().find_map(|slot| match slot.get() {
            ThreadHeap::Vacant => None,
            heap @ ThreadHeap::Active { .. } => Some(heap),
        })
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

    /// Freelist empty: `extend`, accept inbox if needed, then local/OS `acquire_run`.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        class: SizeClass,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(heap) = self.heap() else {
            return Ok(None);
        };
        if let Some(ptr) = self.extend_current(class) {
            return Ok(Some(ptr));
        }
        let mut inner = heap.require_inner();
        heap.flush(&mut inner, ctx, None)?;
        let Some(run) = inner.acquire_run(class, ctx.pages, heap) else {
            return Ok(None);
        };
        self.set_current(class, Some(run));
        Ok(self.extend_current(class))
    }

    fn extend_current(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = self.current(class)?;
        run.allocate().or_else(|| {
            run.extend();
            run.allocate()
        })
    }

    /// Owner-local large allocation via an attached heap.
    ///
    /// Returns `None` when this thread has no attached heap (caller should `bind`).
    #[inline(never)]
    pub(crate) fn alloc_extent(
        &self,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(heap) = self.heap() else {
            return Ok(None);
        };
        let mut inner = heap.require_inner();
        heap.alloc_extent(&mut inner, spec, init, ctx)
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
    /// Reuses an already attached heap; otherwise acquires into the first
    /// vacant slot (Heaps locks internally).
    #[cold]
    pub(crate) fn bind(&self, ctx: &AllocatorCtx<'static>) -> Option<HeapId> {
        #[cfg(not(feature = "c-abi"))]
        Self::arm_exit();
        #[cfg(feature = "c-abi")]
        self.arm_exit();
        if let Some(id) = self.active().and_then(ThreadHeap::id) {
            return Some(id);
        }

        let heap = ctx.heaps.acquire()?;
        let Some(slot) = self.vacant() else {
            Allocator::abort();
        };
        slot.set(ThreadHeap::active(heap));
        Some(heap.id())
    }

    /// First Draining freer becomes Active owner. A third heap stays on
    /// `Heaps::free` until a slot is unbound.
    #[cold]
    pub(crate) fn adopt(&self, heap: &'static Heap, ctx: &AllocatorCtx) -> bool {
        #[cfg(not(feature = "c-abi"))]
        Self::arm_exit();
        #[cfg(feature = "c-abi")]
        self.arm_exit();
        if self.owns(heap) {
            return true;
        }
        let Some(slot) = self.vacant() else {
            return false;
        };
        let Ok(mut inner) = heap.adopt(heap.id()) else {
            return false;
        };
        slot.set(ThreadHeap::active(heap));
        if heap.flush(&mut inner, ctx, None).is_err() {
            Allocator::abort();
        }
        true
    }

    fn unbind_slot(&self, slot: &Cell<ThreadHeap>, ctx: &AllocatorCtx) {
        let ThreadHeap::Active { heap, id } = slot.replace(ThreadHeap::Vacant) else {
            return;
        };
        self.release_current(heap);
        if ctx.heaps.unbind(id, ctx).is_err() {
            Allocator::abort();
        }
    }

    /// Idle extra heap: reclaim an attached heap with no live work, but keep the
    /// last slot so retained empty runs stay on an attached heap.
    fn idle(&self, owner: &Heap) -> Option<&Cell<ThreadHeap>> {
        let attached = self
            .heaps
            .iter()
            .filter(|slot| slot.get().heap().is_some())
            .count();
        if attached <= 1 {
            return None;
        }
        self.heaps.iter().find(|slot| {
            let heap = slot.get();
            heap.owns(owner) && heap.is_idle()
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
                    && let Some(slot) = self.idle(run.heap())
                {
                    self.unbind_slot(slot, ctx);
                }
                Ok(())
            }
            PageOwner::Extent(extent) => {
                self.free_extent(extent, ptr, ctx)?;
                if let Some(slot) = self.idle(extent.heap()) {
                    self.unbind_slot(slot, ctx);
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
        self.active().and_then(ThreadHeap::heap)
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
        for slot in &self.heaps {
            self.unbind_slot(slot, ctx);
        }
    }

    fn exit(&self) {
        let Some(ctx) = Allocator::ctx() else {
            if self
                .heaps
                .iter()
                .any(|slot| !matches!(slot.get(), ThreadHeap::Vacant))
            {
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
