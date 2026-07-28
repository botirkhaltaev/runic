use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::{Allocator, AllocatorInner},
    heap::{Extent, ExtentInit, HeapError, HeapId, Run, RunError},
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

use super::Heap;

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`.
    Remote,
    Heap(HeapError),
}

impl From<RunError> for ThreadFreeError {
    fn from(error: RunError) -> Self {
        Self::Heap(error.into())
    }
}

/// Thread-local frontend: bound heap and cached runs.
///
/// Hot paths take `NonNull<AllocatorInner>` for identity and `&PageMap` projected once
/// at the `Allocator` boundary (avoids parent+field dual refs inside TLS).
/// Sticky hit paths use no locks and no atomics.
pub(crate) struct ThreadHeap {
    inner: Cell<*mut AllocatorInner>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    runs: [Cell<*mut Run>; SizeClasses::COUNT],
    /// Last cached run page number (`usize::MAX` = empty). See `lookup`.
    page_cache_page: Cell<usize>,
    page_cache_owner: Cell<Option<PageOwner>>,
}

impl Drop for ThreadHeap {
    fn drop(&mut self) {
        self.unbind();
    }
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            inner: Cell::new(core::ptr::null_mut()),
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            runs: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
            page_cache_page: Cell::new(usize::MAX),
            page_cache_owner: Cell::new(None),
        }
    }

    /// `PageMap` lookup with a one-entry TLS page→run cache (miss fills from `pages`).
    ///
    /// Cache hit/fill only while bound to `inner` (retained allocator). Only **run**
    /// owners are cached: runs stay published for the heap lifetime in v0.5. Extents
    /// may `unpublish` / reuse VA, so caching them would go stale.
    #[inline]
    pub(crate) fn lookup(
        &self,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
        ptr: NonNull<u8>,
    ) -> Option<PageOwner> {
        let page = ptr.as_ptr().addr() / crate::memory::PAGE_SIZE;
        let bound = self.matches(inner);
        if bound && self.page_cache_page.get() == page {
            return self.page_cache_owner.get();
        }

        let owner = pages.get(ptr)?;
        if bound && matches!(owner, PageOwner::Run(_)) {
            self.page_cache_page.set(page);
            self.page_cache_owner.set(Some(owner));
        }
        Some(owner)
    }

    /// Owner-local small allocation via the TLS run cache.
    ///
    /// Returns `None` when this thread is not bound to `inner` (caller should `bind`).
    /// Sticky hit is the straight-line body; empty sticky goes through `refill`.
    pub(crate) fn alloc(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }

        let cell = self.run_cell(class);

        if let Some(run) = NonNull::new(cell.get()) {
            // SAFETY: sticky run pointers are published from this heap's live arena.
            match unsafe { run.as_ref() }.allocate() {
                Some(ptr) => return Some(ptr),
                None => cell.set(core::ptr::null_mut()),
            }
        }

        self.refill(class, pages, self.bound_heap())
    }

    /// Owner-local large allocation via the bound heap (no sticky extent cache).
    ///
    /// Returns `None` when this thread is not bound to `inner` (caller should `bind`).
    pub(crate) fn alloc_extent(
        &self,
        inner: NonNull<AllocatorInner>,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }

        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().alloc_extent(spec, pages, init) }
    }

    /// Unbound cold path after `bind`: flush-then-alloc (run or extent).
    #[cold]
    pub(crate) fn alloc_fresh(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().alloc_run(class, pages) }
    }

    /// Unbound cold path after `bind`: flush-then-alloc for an extent.
    #[cold]
    pub(crate) fn alloc_extent_fresh(
        &self,
        inner: NonNull<AllocatorInner>,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().alloc_extent(spec, pages, init) }
    }

    /// Owner-local free for a run owned by the bound heap.
    ///
    /// Sticky hit is the straight-line body (unbind clears sticky ⇒ sticky implies bound);
    /// non-cached owner free goes through `Heap::free` after `matches` / `HeapId`.
    #[inline]
    pub(crate) fn free_run(
        &self,
        inner: NonNull<AllocatorInner>,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_ref() };
        let class = run_ref.class();
        // Sticky before matches: cells only park this heap's runs and are cleared on unbind.
        if self.run_cell(class).get() == run.as_ptr() {
            // Sticky hit: Run only — no available-list relink.
            return run_ref.free(ptr).map_err(ThreadFreeError::from);
        }

        if !self.matches(inner) || self.heap_id.get() != Some(run_ref.heap_id()) {
            return Err(ThreadFreeError::Remote);
        }

        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().free(PageOwner::Run(run), ptr, pages) }
            .map_err(ThreadFreeError::Heap)
    }

    /// Owner-local free for an extent owned by the bound heap.
    pub(crate) fn free_extent(
        &self,
        inner: NonNull<AllocatorInner>,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        if !self.matches(inner) || self.heap_id.get() != Some(heap_id) {
            return Err(ThreadFreeError::Remote);
        }

        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().free(PageOwner::Extent(extent), ptr, pages) }
            .map_err(ThreadFreeError::Heap)
    }

    /// Bind this thread to a heap in `Heaps`.
    ///
    /// Reuses the current binding when already attached to `inner`; otherwise unbinds any
    /// foreign binding and acquires a fresh heap (Heaps locks internally).
    pub(crate) fn bind(&self, inner: NonNull<AllocatorInner>) -> Option<HeapId> {
        if self.matches(inner) {
            return self.heap_id.get();
        }

        if !self.is_empty() {
            self.unbind();
        }

        if !AllocatorInner::retain(inner) {
            return None;
        }

        // SAFETY: retain succeeded; Heaps lives for the retained lifetime.
        let acquired = unsafe { inner.as_ref() }.heaps.acquire();
        let Some((id, heap)) = acquired else {
            AllocatorInner::release(inner);
            return None;
        };
        self.install(inner, heap, id);

        Some(id)
    }

    fn install(&self, inner: NonNull<AllocatorInner>, heap: NonNull<Heap>, id: HeapId) {
        self.heap.set(heap.as_ptr());
        self.heap_id.set(Some(id));
        self.inner.set(inner.as_ptr());
    }

    fn matches(&self, inner: NonNull<AllocatorInner>) -> bool {
        self.inner.get() == inner.as_ptr()
    }

    /// No allocator retain — never bound, or after `unbind`.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.get().is_null()
    }

    /// Sticky empty: prefer local/OS runs, then flush inbox and retry.
    #[cold]
    fn refill(
        &self,
        class: SizeClass,
        pages: &PageMap,
        heap: NonNull<Heap>,
    ) -> Option<NonNull<u8>> {
        let cell = self.run_cell(class);

        // SAFETY: Active TLS owner for this bound heap.
        let heap_ref = unsafe { heap.as_ref() };

        // Prefer bump/available/OS before remote accept so a fast lock-free fan-in does not
        // force the owner through flush on every sticky-empty refill.
        let install = |run: NonNull<Run>| -> Option<NonNull<u8>> {
            cell.set(run.as_ptr());
            // SAFETY: run was just returned by this heap's live arena.
            if let Some(ptr) = unsafe { run.as_ref() }.allocate() {
                return Some(ptr);
            }
            cell.set(core::ptr::null_mut());
            // Do not abandon a checked-out run off the available list.
            // SAFETY: Active TLS owner returning a run acquired from this heap.
            let _ = unsafe { heap_ref.push_available(run) };
            None
        };

        // SAFETY: Active TLS owner. Inbox flush is deferred until local acquire fails.
        if let Some(run) = unsafe { heap_ref.acquire_run(class, pages) }
            && let Some(ptr) = install(run)
        {
            return Some(ptr);
        }

        // Always flush (empty drain is cheap) then retry — never early-None on a stale empty check
        // while a concurrent Active publish may still land.
        // SAFETY: Active TLS owner.
        unsafe { heap_ref.flush(pages) }.ok()?;

        // SAFETY: Active TLS owner.
        let run = unsafe { heap_ref.acquire_run(class, pages) }?;
        install(run)
    }

    fn run_cell(&self, class: SizeClass) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.runs.len());
        // SAFETY: SizeClass values are created only by SizeClasses for indexes in this array.
        unsafe { self.runs.get_unchecked(class.index()) }
    }

    /// Bound heap pointer after a successful `matches` / heap-id check.
    fn bound_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS entry matched a bound allocator inner.
        unsafe { NonNull::new_unchecked(heap) }
    }

    fn clear_runs(&self) {
        let Some(heap) = NonNull::new(self.heap.get()) else {
            return;
        };

        for run in &self.runs {
            let Some(run) = NonNull::new(run.replace(core::ptr::null_mut())) else {
                continue;
            };

            // SAFETY: Active TLS owner until install fields are cleared in unbind.
            let _ = unsafe { heap.as_ref().push_available(run) };
        }
    }

    /// Retire the bound heap and release the inner retain.
    #[cold]
    pub(crate) fn unbind(&self) {
        self.clear_runs();
        self.page_cache_page.set(usize::MAX);
        self.page_cache_owner.set(None);
        let Some(inner) = NonNull::new(self.inner.replace(core::ptr::null_mut())) else {
            return;
        };
        let heap_id = self.heap_id.replace(None);
        self.heap.set(core::ptr::null_mut());

        if let Some(heap_id) = heap_id {
            // SAFETY: this TLS entry retained inner while bound; project then drop before release.
            let retired = unsafe {
                let inner = inner.as_ref();
                inner.heaps.retire(heap_id, inner.pages())
            };
            if retired.is_err() {
                Allocator::abort();
            }
        }

        AllocatorInner::release(inner);
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
}
