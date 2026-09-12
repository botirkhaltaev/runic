use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::Allocator,
    heap::{Extent, ExtentInit, HeapError, HeapId, Run, RunError},
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

use super::{AllocatorCtx, Heap};

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`
    /// with the `PageOwner` `free_slow` already looked up.
    Remote(PageOwner),
    Heap(HeapError),
}

/// Thread-local frontend: bound heap, at most one adopted heap, per-class current run.
///
/// Hit is current-run pop / `Run::release`. Miss / bind / unbind / adopt take
/// [`AllocatorCtx`]. `lookup` is miss / realloc. `alloc` never uses the adopted heap.
pub(crate) struct ThreadHeap {
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    adopted_id: Cell<Option<HeapId>>,
    adopted: Cell<*mut Heap>,
    current: [Cell<*mut Run>; SizeClasses::COUNT],
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            adopted_id: Cell::new(None),
            adopted: Cell::new(core::ptr::null_mut()),
            current: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
        }
    }

    fn owns(&self, id: HeapId) -> bool {
        self.heap_id.get() == Some(id) || self.adopted_id.get() == Some(id)
    }

    /// Small: in-page header. Else `PageMap` (extents / fallback).
    pub(crate) fn lookup(pages: &PageMap, ptr: NonNull<u8>, spec: LayoutSpec) -> Option<PageOwner> {
        if SizeClasses::class_for(spec).is_some()
            && let Some(run) = Run::header_of(ptr)
        {
            return Some(PageOwner::Run(run));
        }
        pages.get(ptr)
    }

    /// Owner-local small allocation via the current run for `class`.
    ///
    /// Hit is pop. Empty / unbound → caller miss.
    #[inline]
    pub(crate) fn alloc(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = NonNull::new(self.current(class).get())?;
        // SAFETY: `current` stores only live arena run pointers while bound.
        unsafe { run.as_ref().allocate() }
    }

    /// Owner-local small free via the current run for `class`.
    ///
    /// Hit is `Run::release` (`locate` + push). `OutOfRange` / unbound → caller
    /// `dealloc_slow`. Interior is `InvalidPointer` → abort.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>, class: SizeClass) -> Option<()> {
        let run = NonNull::new(self.current(class).get())?;
        // SAFETY: `current` stores only live arena run pointers while bound.
        match unsafe { run.as_ref() }.release(ptr) {
            Ok(()) => Some(()),
            Err(RunError::OutOfRange) => None,
            Err(_) => Allocator::abort(),
        }
    }

    /// Freelist empty: `extend`, accept inbox if needed, then local/OS `acquire_run`.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        class: SizeClass,
        ctx: &AllocatorCtx<'_>,
    ) -> Option<NonNull<u8>> {
        if self.is_empty() {
            return None;
        }
        if let Some(ptr) = self.extend_current(class) {
            return Some(ptr);
        }
        let heap = self.bound_heap();
        let mut inner = heap.require_inner();
        // Same as `Heap::alloc_extent`: accept remote claims before mapping another run.
        // Mapping first fills the run arena with claimed-full runs and `alloc` returns null.
        if !heap.inboxes_empty() {
            heap.flush(&mut inner, ctx).ok()?;
        }
        if let Some(run) = inner.acquire_run(class, ctx.pages, heap) {
            return self.install_current(class, run);
        }
        None
    }

    fn extend_current(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = NonNull::new(self.current(class).get())?;
        // SAFETY: `current` stores only live arena run pointers while bound.
        let run = unsafe { run.as_ref() };
        if run.extend() {
            return run.allocate();
        }
        None
    }

    fn install_current(&self, class: SizeClass, run: NonNull<Run>) -> Option<NonNull<u8>> {
        self.current(class).set(run.as_ptr());
        // SAFETY: run was just returned by this heap's live arena.
        let run_ref = unsafe { run.as_ref() };
        if let Some(ptr) = run_ref.allocate() {
            return Some(ptr);
        }
        if run_ref.extend() {
            return run_ref.allocate();
        }
        None
    }

    /// Owner-local large allocation via the bound heap.
    ///
    /// Returns `None` when this thread is not bound (caller should `bind`).
    #[inline(never)]
    pub(crate) fn alloc_extent(
        &self,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx<'_>,
    ) -> Option<NonNull<u8>> {
        if self.is_empty() {
            return None;
        }
        let heap = self.bound_heap();
        let mut inner = heap.require_inner();
        heap.alloc_extent(&mut inner, spec, init, ctx)
    }

    /// Owner drain of remote-free inboxes (Active TLS).
    pub(crate) fn flush(&self, ctx: &AllocatorCtx<'_>) -> Result<(), HeapError> {
        if self.is_empty() {
            return Err(HeapError::InvalidHeap);
        }
        let heap = self.bound_heap();
        let mut inner = heap.require_inner();
        heap.flush(&mut inner, ctx)
    }

    /// Available-list insert after a full run took a free. Off the free hit.
    #[inline(never)]
    pub(crate) fn push_available(&self, run: NonNull<Run>) {
        // SAFETY: caller just owner-freed `run` on a heap this TLS owns.
        let heap_id = unsafe { run.as_ref() }.heap_id();
        let heap = self.owned_heap(heap_id);
        let mut inner = heap.require_inner();
        if inner.push_available(run).is_err() {
            Allocator::abort();
        }
    }

    /// Owner-local free for a run owned by the bound or adopted heap.
    ///
    /// `Run::free` is lock-free. `push_available` only when the run was full.
    /// Heap-id stays: `lookup` can still return a foreign run.
    #[inline]
    pub(crate) fn free_run(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: caller supplies a PageMap / arena run pointer.
        let run_ref = unsafe { run.as_ref() };
        if !self.owns(run_ref.heap_id()) {
            return Err(ThreadFreeError::Remote(PageOwner::Run(run)));
        }
        match run_ref.free(ptr) {
            Ok(false) => Ok(()),
            Ok(true) => {
                self.push_available(run);
                Ok(())
            }
            Err(_) => Allocator::abort(),
        }
    }

    /// Owner-local free for an extent owned by the bound or adopted heap.
    pub(crate) fn free_extent(
        &self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        if !self.owns(heap_id) {
            return Err(ThreadFreeError::Remote(PageOwner::Extent(extent)));
        }

        let heap = self.owned_heap(heap_id);
        let mut inner = heap.require_inner();
        inner
            .free(PageOwner::Extent(extent), ptr, ctx, heap)
            .map(|_| ())
            .map_err(ThreadFreeError::Heap)
    }

    /// Bind this thread to a heap in `ctx`.
    ///
    /// Reuses the current binding when already attached; otherwise acquires a
    /// fresh heap (Heaps locks internally).
    #[cold]
    pub(crate) fn bind(&self, ctx: &AllocatorCtx<'_>) -> Option<HeapId> {
        UNBIND_GUARD.with(|_| {});
        if !self.is_empty() {
            return self.heap_id.get();
        }

        let heap = ctx.heaps.acquire()?;
        self.install(heap);
        Some(heap.id())
    }

    fn install(&self, heap: &Heap) {
        self.heap.set(core::ptr::from_ref(heap).cast_mut());
        self.heap_id.set(Some(heap.id()));
    }

    /// No bound heap. An adopted heap may still be set.
    pub(crate) fn is_empty(&self) -> bool {
        self.heap.get().is_null()
    }

    fn install_adopted(&self, heap: &Heap) {
        self.adopted.set(core::ptr::from_ref(heap).cast_mut());
        self.adopted_id.set(Some(heap.id()));
    }

    /// First Draining freer becomes Active owner. One adopted heap; a different
    /// heap stays on `Heaps::free` until this slot is retired at unbind.
    #[cold]
    pub(crate) fn adopt(&self, heap: &Heap, id: HeapId, ctx: &AllocatorCtx<'_>) -> bool {
        UNBIND_GUARD.with(|_| {});
        if self.adopted_id.get() == Some(id) {
            return true;
        }
        if self.adopted_id.get().is_some() {
            return false;
        }
        if heap.adopt(id).is_err() {
            return false;
        }
        self.install_adopted(heap);
        let mut inner = heap.lock_inner();
        if heap.flush(&mut inner, ctx).is_err() {
            Allocator::abort();
        }
        true
    }

    /// Close, flush, and reclaim the adopted heap. Slot is empty afterwards.
    #[cold]
    pub(crate) fn retire_adopted(&self, ctx: &AllocatorCtx<'_>) {
        let Some(id) = self.adopted_id.replace(None) else {
            return;
        };
        self.adopted.set(core::ptr::null_mut());
        match ctx.heaps.retire(id, ctx) {
            Ok(()) | Err(HeapError::InvalidHeap) => {}
            Err(_) => Allocator::abort(),
        }
    }

    /// Retire the adopted heap when `id` is that heap and it has no live blocks.
    fn retire_if_idle(&self, id: HeapId, ctx: &AllocatorCtx<'_>) {
        if self.adopted_id.get() != Some(id) {
            return;
        }
        let Some(heap) = self.adopted_heap() else {
            return;
        };
        if heap.inboxes_empty() && !heap.has_live() {
            self.retire_adopted(ctx);
        }
    }

    fn adopted_heap(&self) -> Option<&Heap> {
        // SAFETY: adopted pointer is a live arena heap while the slot is set.
        Some(unsafe { NonNull::new(self.adopted.get())?.as_ref() })
    }

    fn owned_heap(&self, id: HeapId) -> &Heap {
        if self.heap_id.get() == Some(id) {
            return self.bound_heap();
        }
        if self.adopted_id.get() == Some(id)
            && let Some(heap) = self.adopted_heap()
        {
            return heap;
        }
        Allocator::abort()
    }

    /// Bound or adopted owner-local free. Reclaim the adopted heap when idle.
    pub(crate) fn free_owner(
        &self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), ThreadFreeError> {
        match owner {
            PageOwner::Run(run) => {
                self.free_run(run, ptr)?;
                if self.adopted_id.get().is_some() {
                    // SAFETY: same live arena run just freed.
                    let run = unsafe { run.as_ref() };
                    if !run.is_live() {
                        self.retire_if_idle(run.heap_id(), ctx);
                    }
                }
                Ok(())
            }
            PageOwner::Extent(extent) => {
                self.free_extent(extent, ptr, ctx)?;
                if self.adopted_id.get().is_some() {
                    // SAFETY: PageMap extent; `heap_id` is stable across free.
                    self.retire_if_idle(unsafe { extent.as_ref() }.heap_id(), ctx);
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
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), ThreadFreeError> {
        let Some(owner) = Self::lookup(ctx.pages, ptr, spec) else {
            return Err(ThreadFreeError::Heap(HeapError::InvalidRunPointer));
        };
        self.free_owner(owner, ptr, ctx)
    }

    fn current(&self, class: SizeClass) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.current.len());
        // SAFETY: SizeClass values are created only by SizeClasses for indexes in this array.
        unsafe { self.current.get_unchecked(class.index()) }
    }

    /// Bound heap after a successful heap-id check.
    fn bound_heap(&self) -> &Heap {
        let Some(heap) = NonNull::new(self.heap.get()) else {
            Allocator::abort();
        };
        // SAFETY: bound pointer is a live arena heap.
        unsafe { heap.as_ref() }
    }

    /// Retire the bound heap. `live` stays exact. The process payload stays.
    ///
    /// Non-full current runs go back on the available list so reincarnation
    /// can reuse them. `push_available` is idempotent if a run is already linked.
    #[cold]
    pub(crate) fn unbind(&self, ctx: &AllocatorCtx<'_>) {
        if !self.is_empty() {
            let heap = self.bound_heap();
            let mut inner = heap.require_inner();
            for cell in &self.current {
                let Some(run) = NonNull::new(cell.get()) else {
                    continue;
                };
                // SAFETY: `current` stores only live arena run pointers while bound.
                if unsafe { run.as_ref() }.is_full() {
                    continue;
                }
                if inner.push_available(run).is_err() {
                    Allocator::abort();
                }
            }
        }
        for cell in &self.current {
            cell.set(core::ptr::null_mut());
        }
        let heap_id = self.heap_id.replace(None);
        self.heap.set(core::ptr::null_mut());
        if let Some(heap_id) = heap_id
            && ctx.heaps.retire(heap_id, ctx).is_err()
        {
            Allocator::abort();
        }
        self.retire_adopted(ctx);
    }
}

/// `LocalKey` so `Drop` retires the heap. `THREAD_HEAP` is `!Drop` ELF TLS.
struct UnbindGuard;

impl Drop for UnbindGuard {
    fn drop(&mut self) {
        let Some(ctx) = Allocator::ctx() else {
            if !THREAD_HEAP.is_empty()
                || THREAD_HEAP.heap_id.get().is_some()
                || THREAD_HEAP.adopted_id.get().is_some()
            {
                Allocator::abort();
            }
            return;
        };
        THREAD_HEAP.unbind(&ctx);
    }
}

#[cfg(test)]
impl ThreadHeap {
    pub(crate) fn adopted_id_for_test(&self) -> Option<HeapId> {
        self.adopted_id.get()
    }
}

#[thread_local]
pub(crate) static THREAD_HEAP: ThreadHeap = ThreadHeap::new();

std::thread_local! {
    static UNBIND_GUARD: UnbindGuard = const { UnbindGuard };
}
