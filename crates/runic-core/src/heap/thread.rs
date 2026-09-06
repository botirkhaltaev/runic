use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::{Allocator, AllocatorInner},
    heap::{Extent, ExtentInit, HeapError, HeapId, Run},
    layout::LayoutSpec,
    memory::{PAGE_SIZE, PageMap, PageOwner},
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

/// Thread-local frontend: bound heap and per-class current run.
///
/// Hot paths take `NonNull<AllocatorInner>` for identity. `&PageMap` is projected
/// only on miss. Hit is current-run pop / page-cache `Run::free`.
pub(crate) struct ThreadHeap {
    inner: Cell<*mut AllocatorInner>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    current: [Cell<*mut Run>; SizeClasses::COUNT],
    /// Last cached run page number (`usize::MAX` = empty).
    page_cache_page: Cell<usize>,
    /// Cached own-heap run pointer (`null` = empty). Extents are never cached.
    page_cache_run: Cell<*mut Run>,
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            inner: Cell::new(core::ptr::null_mut()),
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            current: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
            page_cache_page: Cell::new(usize::MAX),
            page_cache_run: Cell::new(core::ptr::null_mut()),
        }
    }

    /// `PageMap` lookup with a one-entry TLS page→run cache (own-heap runs only).
    fn lookup(
        &self,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
        ptr: NonNull<u8>,
    ) -> Option<PageOwner> {
        let page = ptr.as_ptr().addr() / PAGE_SIZE;
        if self.matches(inner) && self.page_cache_page.get() == page {
            let run = self.page_cache_run.get();
            if !run.is_null() {
                // SAFETY: cache stores only live own-heap arena run pointers while bound.
                return Some(PageOwner::Run(unsafe { NonNull::new_unchecked(run) }));
            }
        }
        let owner = pages.get(ptr)?;
        if self.matches(inner)
            && let PageOwner::Run(run) = owner
        {
            // SAFETY: PageMap stores only live arena run pointers.
            if self.heap_id.get() == Some(unsafe { run.as_ref() }.heap_id()) {
                self.page_cache_page.set(page);
                self.page_cache_run.set(run.as_ptr());
            }
        }
        Some(owner)
    }

    /// Owner-local small allocation via the current run for `class`.
    ///
    /// Hit is `matches` + pop. Empty / unbound → caller `alloc_miss` / `bind`.
    #[inline]
    pub(crate) fn alloc(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        let run = self.current(class).get();
        if run.is_null() {
            return None;
        }
        // SAFETY: `current` stores only live arena run pointers while bound.
        unsafe { (*run).allocate() }
    }

    /// Freelist empty: `extend`, else `acquire_run`, else flush then retry.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        if let Some(ptr) = self.extend_current(class) {
            return Some(ptr);
        }
        // SAFETY: caller is the Active TLS owner; inner is retained while bound.
        let pages = unsafe { inner.as_ref() }.pages();
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        let heap_ref = unsafe { heap.as_ref() };
        // SAFETY: Active TLS owner. Inbox flush is deferred until local acquire fails.
        if let Some(run) = unsafe { heap_ref.acquire_run(class, pages) } {
            return self.install_current(class, run);
        }
        // SAFETY: Active TLS owner.
        if unsafe { heap_ref.flush(pages) }.is_err() {
            return None;
        }
        // SAFETY: Active TLS owner.
        if let Some(run) = unsafe { heap_ref.acquire_run(class, pages) } {
            return self.install_current(class, run);
        }
        None
    }

    fn extend_current(&self, class: SizeClass) -> Option<NonNull<u8>> {
        let run = self.current(class).get();
        if run.is_null() {
            return None;
        }
        // SAFETY: `current` stores only live arena run pointers while bound.
        let run_ref = unsafe { &*run };
        if run_ref.extend() {
            return run_ref.allocate();
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
    /// Returns `None` when this thread is not bound to `inner` (caller should `bind`).
    #[inline(never)]
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

    /// Unbound path after `bind`: flush inboxes, then owner-local run alloc.
    #[cold]
    pub(crate) fn alloc_after_bind(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        // SAFETY: caller retains `inner` for this unbound alloc.
        let pages = unsafe { inner.as_ref() }.pages();
        self.flush(inner, pages).ok()?;
        self.alloc_miss(inner, class)
    }

    /// Unbound path after `bind`: flush inboxes, then owner-local extent alloc.
    #[cold]
    pub(crate) fn alloc_extent_after_bind(
        &self,
        inner: NonNull<AllocatorInner>,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        self.flush(inner, pages).ok()?;
        self.alloc_extent(inner, spec, pages, init)
    }

    /// Owner drain of remote-free inboxes (Active TLS).
    pub(crate) fn flush(
        &self,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.matches(inner) {
            return Err(HeapError::InvalidHeap);
        }
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        unsafe { heap.as_ref().flush(pages) }
    }

    /// Page-cache own-heap run for `ptr`, if this TLS is bound to `inner`.
    ///
    /// Tiny so `LocalKey::with` inlines; the caller runs `Heap::free_run`.
    #[inline]
    pub(crate) fn cached_run(
        &self,
        inner: NonNull<AllocatorInner>,
        ptr: NonNull<u8>,
    ) -> Option<(NonNull<Heap>, NonNull<Run>)> {
        if !self.matches(inner) {
            return None;
        }
        let page = ptr.as_ptr().addr() / PAGE_SIZE;
        if self.page_cache_page.get() != page {
            return None;
        }
        let run = self.page_cache_run.get();
        if run.is_null() {
            return None;
        }
        // SAFETY: cache stores only live own-heap arena run pointers while bound.
        Some((self.bound_heap(), unsafe { NonNull::new_unchecked(run) }))
    }

    /// Owner-local free for a run owned by the bound heap.
    #[inline]
    pub(crate) fn free_run(
        &self,
        inner: NonNull<AllocatorInner>,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        if !self.matches(inner) {
            return Err(ThreadFreeError::Remote);
        }
        // SAFETY: caller supplies a PageMap / arena run pointer.
        if self.heap_id.get() != Some(unsafe { run.as_ref() }.heap_id()) {
            return Err(ThreadFreeError::Remote);
        }
        self.owner_free(run, ptr);
        Ok(())
    }

    #[inline]
    fn owner_free(&self, run: NonNull<Run>, ptr: NonNull<u8>) {
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner; `run` is a live arena run.
        if unsafe { heap.as_ref().free_run(run, ptr) }.is_err() {
            Allocator::abort();
        }
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
    #[cold]
    pub(crate) fn bind(&self, inner: NonNull<AllocatorInner>) -> Option<HeapId> {
        UNBIND_GUARD.with(|_| {});
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

    /// Cache miss / extent / unbound: `PageMap` then typed free.
    #[inline(never)]
    pub(crate) fn free_slow(
        &self,
        inner: NonNull<AllocatorInner>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: caller retains `inner` for this free.
        let pages = unsafe { inner.as_ref() }.pages();
        let Some(owner) = self.lookup(inner, pages, ptr) else {
            return Err(ThreadFreeError::Heap(HeapError::InvalidRunPointer));
        };
        match owner {
            PageOwner::Run(run) => self.free_run(inner, run, ptr),
            PageOwner::Extent(extent) => self.free_extent(inner, extent, ptr, pages),
        }
    }

    fn current(&self, class: SizeClass) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.current.len());
        // SAFETY: SizeClass values are created only by SizeClasses for indexes in this array.
        unsafe { self.current.get_unchecked(class.index()) }
    }

    /// Bound heap pointer after a successful `matches` / heap-id check.
    fn bound_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS entry matched a bound allocator inner.
        unsafe { NonNull::new_unchecked(heap) }
    }

    /// Retire the bound heap and release the inner retain. `live` stays exact.
    #[cold]
    pub(crate) fn unbind(&self) {
        for cell in &self.current {
            cell.set(core::ptr::null_mut());
        }
        self.page_cache_page.set(usize::MAX);
        self.page_cache_run.set(core::ptr::null_mut());
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

/// Drops after `THREAD_HEAP` is initialized so thread exit still retires the heap.
struct UnbindGuard;

impl Drop for UnbindGuard {
    fn drop(&mut self) {
        THREAD_HEAP.with(ThreadHeap::unbind);
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
    static UNBIND_GUARD: UnbindGuard = const { UnbindGuard };
}
