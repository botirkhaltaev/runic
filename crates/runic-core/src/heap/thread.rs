use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::Allocator,
    heap::{Extent, ExtentInit, HeapError, HeapId, Run, RunCache},
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

/// Thread-local frontend: bound heap and per-class current run.
///
/// Hit is current-run pop / `RunCache` `Run::free`. Miss / bind / unbind take
/// [`AllocatorCtx`].
pub(crate) struct ThreadHeap {
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    current: [Cell<*mut Run>; SizeClasses::COUNT],
    cache: RunCache,
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            current: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
            cache: RunCache::new(),
        }
    }

    /// Owner-local then `PageMap`: `RunCache` → `current[class]` → pages.
    pub(crate) fn lookup(
        &self,
        pages: &PageMap,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Option<PageOwner> {
        if let Some(run) = self.cache.hit(ptr) {
            return Some(PageOwner::Run(run));
        }
        if let Some(class) = SizeClasses::class_for(spec)
            && let Some(run) = NonNull::new(self.current(class).get())
        {
            // SAFETY: `current` stores only live arena run pointers while bound.
            let run_ref = unsafe { run.as_ref() };
            if self.heap_id.get() == Some(run_ref.heap_id())
                && run_ref.range().offset_of(ptr).is_some()
            {
                self.cache.store(run);
                return Some(PageOwner::Run(run));
            }
        }
        let owner = pages.get(ptr)?;
        if let PageOwner::Run(run) = owner {
            // SAFETY: PageMap stores only live arena run pointers.
            if self.heap_id.get() == Some(unsafe { run.as_ref() }.heap_id()) {
                self.cache.store(run);
            }
        }
        Some(owner)
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
        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        // Same as `Heap::alloc_extent`: accept remote claims before mapping another run.
        // Mapping first fills the run arena with claimed-full runs and `alloc` returns null.
        if !heap.inboxes_empty() {
            heap.flush(&mut inner, ctx).ok()?;
        }
        if let Some(run) = heap.acquire_run(&mut inner, class, ctx) {
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
        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.alloc_extent(&mut inner, spec, init, ctx)
    }

    /// Owner drain of remote-free inboxes (Active TLS).
    pub(crate) fn flush(&self, ctx: &AllocatorCtx<'_>) -> Result<(), HeapError> {
        if self.is_empty() {
            return Err(HeapError::InvalidHeap);
        }
        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.flush(&mut inner, ctx)
    }

    /// Available-list insert after a full run took a free. Off the free hit.
    #[inline(never)]
    pub(crate) fn push_available(&self, run: NonNull<Run>) {
        // SAFETY: caller just owner-freed `run` on this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        if heap.push_available(&mut inner, run).is_err() {
            Allocator::abort();
        }
    }

    /// Owner-local free for a run owned by the bound heap.
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
        if self.heap_id.get() != Some(run_ref.heap_id()) {
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

    /// Owner-local free for an extent owned by the bound heap.
    pub(crate) fn free_extent(
        &self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        if self.heap_id.get() != Some(heap_id) {
            return Err(ThreadFreeError::Remote(PageOwner::Extent(extent)));
        }

        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.free(&mut inner, PageOwner::Extent(extent), ptr, ctx)
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

        let (id, heap) = ctx.heaps.acquire()?;
        self.install(heap, id);
        Some(id)
    }

    fn install(&self, heap: NonNull<Heap>, id: HeapId) {
        self.heap.set(heap.as_ptr());
        self.heap_id.set(Some(id));
    }

    /// Never bound, or after `unbind`.
    pub(crate) fn is_empty(&self) -> bool {
        self.heap.get().is_null()
    }

    /// Cache miss / extent / unbound: `PageMap` then typed free.
    #[inline(never)]
    pub(crate) fn free_slow(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), ThreadFreeError> {
        let Some(owner) = self.lookup(ctx.pages, ptr, spec) else {
            return Err(ThreadFreeError::Heap(HeapError::InvalidRunPointer));
        };
        match owner {
            PageOwner::Run(run) => self.free_run(run, ptr),
            PageOwner::Extent(extent) => self.free_extent(extent, ptr, ctx),
        }
    }

    fn current(&self, class: SizeClass) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.current.len());
        // SAFETY: SizeClass values are created only by SizeClasses for indexes in this array.
        unsafe { self.current.get_unchecked(class.index()) }
    }

    /// Bound heap pointer after a successful heap-id check.
    fn bound_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS entry is bound.
        unsafe { NonNull::new_unchecked(heap) }
    }

    /// Retire the bound heap. `live` stays exact. The process payload stays.
    ///
    /// Non-full current runs go back on the available list so reincarnation
    /// can reuse them. `push_available` is idempotent if a run is already linked.
    #[cold]
    pub(crate) fn unbind(&self, ctx: &AllocatorCtx<'_>) {
        if !self.is_empty() {
            // SAFETY: Active TLS owner for this bound heap.
            let heap = unsafe { self.bound_heap().as_ref() };
            let Some(mut inner) = heap.try_inner() else {
                Allocator::abort();
            };
            for cell in &self.current {
                let Some(run) = NonNull::new(cell.get()) else {
                    continue;
                };
                // SAFETY: `current` stores only live arena run pointers while bound.
                if unsafe { run.as_ref() }.is_full() {
                    continue;
                }
                if heap.push_available(&mut inner, run).is_err() {
                    Allocator::abort();
                }
            }
        }
        for cell in &self.current {
            cell.set(core::ptr::null_mut());
        }
        self.cache.clear();
        let heap_id = self.heap_id.replace(None);
        self.heap.set(core::ptr::null_mut());
        let Some(heap_id) = heap_id else {
            return;
        };

        if ctx.heaps.retire(heap_id, ctx).is_err() {
            Allocator::abort();
        }
    }
}

/// Drops after `THREAD_HEAP` is initialized so thread exit still retires the heap.
struct UnbindGuard;

impl Drop for UnbindGuard {
    fn drop(&mut self) {
        THREAD_HEAP.with(|tls| {
            let Some(ctx) = Allocator::ctx() else {
                if !tls.is_empty() || tls.heap_id.get().is_some() {
                    Allocator::abort();
                }
                return;
            };
            tls.unbind(&ctx);
        });
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
    static UNBIND_GUARD: UnbindGuard = const { UnbindGuard };
}
