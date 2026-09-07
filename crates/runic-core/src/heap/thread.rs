use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::{Allocator, AllocatorInner},
    heap::{Extent, ExtentInit, HeapError, HeapId, Run, RunCache},
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

use super::{Heap, HeapCtx};

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`.
    Remote,
    Heap(HeapError),
}

/// Thread-local frontend: bound heap and per-class current run.
///
/// Hot paths take a raw inner pointer for identity. `&PageMap` is projected
/// only on miss. Hit is current-run pop / `RunCache` `Run::free`.
pub(crate) struct ThreadHeap {
    inner: Cell<*mut AllocatorInner>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    current: [Cell<*mut Run>; SizeClasses::COUNT],
    cache: RunCache,
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            inner: Cell::new(core::ptr::null_mut()),
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            current: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
            cache: RunCache::new(),
        }
    }

    /// `PageMap` lookup with a one-entry TLS run-range cache (own-heap runs only).
    fn lookup(
        &self,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
        ptr: NonNull<u8>,
    ) -> Option<PageOwner> {
        if self.matches(inner.as_ptr())
            && let Some(run) = self.cache.hit(ptr)
        {
            return Some(PageOwner::Run(run));
        }
        let owner = pages.get(ptr)?;
        if self.matches(inner.as_ptr())
            && let PageOwner::Run(run) = owner
        {
            // SAFETY: PageMap stores only live arena run pointers.
            if self.heap_id.get() == Some(unsafe { run.as_ref() }.heap_id()) {
                self.cache.store(run);
            }
        }
        Some(owner)
    }

    /// Owner-local small allocation via the current run for `class`.
    ///
    /// Hit is `matches` + pop. Empty / unbound / uninit → caller miss.
    #[inline]
    pub(crate) fn alloc(
        &self,
        inner: *mut AllocatorInner,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        let run = NonNull::new(self.current(class).get())?;
        // SAFETY: `current` stores only live arena run pointers while bound.
        unsafe { run.as_ref().allocate() }
    }

    /// Freelist empty: `extend`, accept inbox if needed, then local/OS `acquire_run`.
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner.as_ptr()) {
            return None;
        }
        if let Some(ptr) = self.extend_current(class) {
            return Some(ptr);
        }
        // SAFETY: caller is the Active TLS owner; inner is retained while bound.
        let pages = unsafe { inner.as_ref() }.pages();
        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let ctx = HeapCtx { pages };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        // Same as `Heap::alloc_extent`: accept remote claims before mapping another run.
        // Mapping first fills the run arena with claimed-full runs and `alloc` returns null.
        if !heap.inboxes_empty() {
            heap.flush(&mut inner, &ctx).ok()?;
        }
        if let Some(run) = heap.acquire_run(&mut inner, class, &ctx) {
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
    /// Returns `None` when this thread is not bound to `inner` (caller should `bind`).
    #[inline(never)]
    pub(crate) fn alloc_extent(
        &self,
        inner: NonNull<AllocatorInner>,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner.as_ptr()) {
            return None;
        }

        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let ctx = HeapCtx { pages };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.alloc_extent(&mut inner, spec, init, &ctx)
    }

    /// Unbound path after `bind`: flush inboxes, then owner-local run alloc.
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
        if !self.matches(inner.as_ptr()) {
            return Err(HeapError::InvalidHeap);
        }
        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let ctx = HeapCtx { pages };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.flush(&mut inner, &ctx)
    }

    /// [`RunCache`] hit for `ptr`, if this TLS is bound to `inner`.
    #[inline]
    pub(crate) fn cached_run(
        &self,
        inner: *mut AllocatorInner,
        ptr: NonNull<u8>,
    ) -> Option<NonNull<Run>> {
        if !self.matches(inner) {
            return None;
        }
        self.cache.hit(ptr)
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
    /// `Run::free` is lock-free. `push_available` (Inner) only when the run was full.
    /// `matches` / heap-id stay: `lookup` can still return a foreign run.
    #[inline]
    pub(crate) fn free_run(
        &self,
        inner: NonNull<AllocatorInner>,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        if !self.matches(inner.as_ptr()) {
            return Err(ThreadFreeError::Remote);
        }
        // SAFETY: caller supplies a PageMap / arena run pointer.
        let run_ref = unsafe { run.as_ref() };
        if self.heap_id.get() != Some(run_ref.heap_id()) {
            return Err(ThreadFreeError::Remote);
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
        inner: NonNull<AllocatorInner>,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        if !self.matches(inner.as_ptr()) || self.heap_id.get() != Some(heap_id) {
            return Err(ThreadFreeError::Remote);
        }

        // SAFETY: Active TLS owner for this bound heap.
        let heap = unsafe { self.bound_heap().as_ref() };
        let ctx = HeapCtx { pages };
        let Some(mut inner) = heap.try_inner() else {
            Allocator::abort();
        };
        heap.free(&mut inner, PageOwner::Extent(extent), ptr, &ctx)
            .map_err(ThreadFreeError::Heap)
    }

    /// Bind this thread to a heap in `Heaps`.
    ///
    /// Reuses the current binding when already attached to `inner`; otherwise unbinds any
    /// foreign binding and acquires a fresh heap (Heaps locks internally).
    #[cold]
    pub(crate) fn bind(&self, inner: NonNull<AllocatorInner>) -> Option<HeapId> {
        UNBIND_GUARD.with(|_| {});
        if self.matches(inner.as_ptr()) {
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

    fn matches(&self, inner: *mut AllocatorInner) -> bool {
        self.inner.get() == inner
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
        self.cache.clear();
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
