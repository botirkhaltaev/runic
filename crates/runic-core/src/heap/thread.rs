use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::{Allocator, AllocatorInner},
    heap::{Extent, ExtentInit, HeapError, HeapId, Run},
    layout::LayoutSpec,
    memory::{PAGE_SIZE, PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

use super::Heap;

/// Magazine high-water. `take` then `Heap::free` when count reaches this.
///
/// Must stay well below the bench `PHASE_BATCH` (512) so `owner_free_only` is
/// not a push-only lie, and so the claim window stays short.
pub(crate) const MAGAZINE_WATERMARK: u32 = 32;

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`.
    Remote,
    Heap(HeapError),
}

/// Per-class lockless TLS magazine: intrusive payload `usize` links, `Cell` only.
struct Magazine {
    head: Cell<Option<NonNull<u8>>>,
    count: Cell<u32>,
}

impl Magazine {
    const fn new() -> Self {
        Self {
            head: Cell::new(None),
            count: Cell::new(0),
        }
    }

    #[inline]
    fn pop(&self) -> Option<NonNull<u8>> {
        let head = self.head.get()?;
        debug_assert!(self.count.get() >= 1);
        self.head.set(Self::read_next(head));
        self.count.set(self.count.get() - 1);
        Some(head)
    }

    #[inline]
    fn push(&self, ptr: NonNull<u8>) {
        debug_assert!(self.count.get() < MAGAZINE_WATERMARK);
        Self::write_next(ptr, self.head.get());
        self.head.set(Some(ptr));
        self.count.set(self.count.get() + 1);
    }

    #[inline]
    fn read_next(ptr: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: magazine-resident blocks are owner-local payloads of size ≥ 8.
        // The first usize is the next link written by `write_next` (0 = end).
        let next = unsafe { ptr.cast::<usize>().read() };
        NonNull::new(core::ptr::without_provenance_mut(next))
    }

    #[inline]
    fn write_next(ptr: NonNull<u8>, next: Option<NonNull<u8>>) {
        let word = next.map_or(0, |head| head.as_ptr().addr());
        // SAFETY: owner just freed this block, or refill just allocated it and it
        // is not yet user-visible. First usize is the intrusive magazine link.
        unsafe { ptr.cast::<usize>().write(word) };
    }

    /// Move `head`/`count` out so the source is empty.
    fn take(&self) -> Self {
        Self {
            head: Cell::new(self.head.take()),
            count: Cell::new(self.count.replace(0)),
        }
    }

    /// Refill: batch `Run::allocate` until watermark−1.
    fn allocate(&self, run: &Run) {
        while self.count.get() + 1 < MAGAZINE_WATERMARK {
            match run.allocate() {
                Some(ptr) => self.push(ptr),
                None => break,
            }
        }
    }
}

impl Iterator for Magazine {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<NonNull<u8>> {
        // Count is the walk bound: a cycled list (owner double-push) stops here.
        if self.count.get() == 0 {
            self.head.set(None);
            return None;
        }
        self.pop()
    }
}

/// Thread-local frontend: bound heap and per-class magazines.
///
/// Hot paths take `NonNull<AllocatorInner>` for identity. `&PageMap` is projected
/// only on miss / take / refill (avoids parent+field dual refs on the hit).
/// Hit paths are magazine pop/push only: no locks, no atomics, no `Run` body.
pub(crate) struct ThreadHeap {
    inner: Cell<*mut AllocatorInner>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    magazines: [Magazine; SizeClasses::COUNT],
    /// Last cached run page number (`usize::MAX` = empty).
    page_cache_page: Cell<usize>,
    /// Cached run pointer (`null` = empty). Runs only — extents are never cached.
    page_cache_run: Cell<*mut Run>,
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
            magazines: [const { Magazine::new() }; SizeClasses::COUNT],
            page_cache_page: Cell::new(usize::MAX),
            page_cache_run: Cell::new(core::ptr::null_mut()),
        }
    }

    /// `PageMap` lookup with a one-entry TLS page→run cache (miss fills from `pages`).
    ///
    /// Cache hit/fill only while bound to `inner` (retained allocator). Only **run**
    /// owners are cached: runs stay published for the heap lifetime in v0.5. Extents
    /// may `unpublish` / reuse VA, so caching them would go stale.
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
                // SAFETY: cache stores only live arena run pointers while bound.
                return Some(PageOwner::Run(unsafe { NonNull::new_unchecked(run) }));
            }
        }
        let owner = pages.get(ptr)?;
        if self.matches(inner)
            && let PageOwner::Run(run) = owner
        {
            self.page_cache_page.set(page);
            self.page_cache_run.set(run.as_ptr());
        }
        Some(owner)
    }

    /// Owner-local small allocation via the TLS magazine.
    ///
    /// Returns `None` when unbound or the magazine is empty (caller `refill` / `bind`).
    /// Hit is `matches` + `pop` only — refill is not in this function.
    #[inline]
    pub(crate) fn alloc(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        self.magazine(class).pop()
    }

    /// Magazine-empty miss: refill then pop. Caller already observed `alloc` miss.
    #[cold]
    #[inline(never)]
    pub(crate) fn alloc_miss(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        if !self.matches(inner) {
            return None;
        }
        self.refill(inner, class);
        self.magazine(class).pop()
    }

    /// Owner-local large allocation via the bound heap (no extent magazine).
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

    /// Unbound cold path after `bind`: flush inboxes, then owner-local run alloc.
    #[cold]
    pub(crate) fn alloc_after_bind(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClass,
    ) -> Option<NonNull<u8>> {
        // SAFETY: caller retains `inner` for this cold unbound alloc.
        let pages = unsafe { inner.as_ref() }.pages();
        self.flush(inner, pages).ok()?;
        self.alloc_miss(inner, class)
    }

    /// Unbound cold path after `bind`: flush inboxes, then owner-local extent alloc.
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

    /// Owner-local free hit: page-cache + heap-id + magazine push.
    ///
    /// Returns `true` when the block was pushed (or taken). `false` means the
    /// caller must run `free_slow` (cache miss, unbound, or foreign heap).
    #[inline]
    pub(crate) fn free_hit(&self, inner: NonNull<AllocatorInner>, ptr: NonNull<u8>) -> bool {
        if !self.matches(inner) {
            return false;
        }
        let page = ptr.as_ptr().addr() / PAGE_SIZE;
        if self.page_cache_page.get() != page {
            return false;
        }
        let run = self.page_cache_run.get();
        if run.is_null() {
            return false;
        }
        // SAFETY: cache stores only live arena run pointers while bound.
        self.push_run(inner, unsafe { NonNull::new_unchecked(run) }, ptr)
            .is_ok()
    }

    /// Owner-local free for a run owned by the bound heap.
    ///
    /// Hit is `matches` + `HeapId` + magazine `push`. `Heap::free` runs on `take`.
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
        self.push_run(inner, run, ptr)
    }

    #[inline]
    fn push_run(
        &self,
        inner: NonNull<AllocatorInner>,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap / cache store only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_ref() };
        if self.heap_id.get() != Some(run_ref.heap_id()) {
            return Err(ThreadFreeError::Remote);
        }

        let mag = self.magazine(run_ref.class());
        mag.push(ptr);
        if mag.count.get() >= MAGAZINE_WATERMARK {
            self.free_magazine(inner, mag.take())?;
        }
        Ok(())
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

    /// Magazine empty: local/OS acquire first; inbox flush only if that misses.
    ///
    /// Deferring accept keeps leftover magazine live on the owner from forcing
    /// a remote drain on every producer refill (fan-in).
    #[cold]
    #[inline(never)]
    fn refill(&self, inner: NonNull<AllocatorInner>, class: SizeClass) {
        // SAFETY: caller is the Active TLS owner; inner is retained while bound.
        let pages = unsafe { inner.as_ref() }.pages();
        let heap = self.bound_heap();
        // SAFETY: Active TLS owner for this bound heap.
        let heap_ref = unsafe { heap.as_ref() };
        let mag = self.magazine(class);

        // SAFETY: Active TLS owner. Inbox flush is deferred until local acquire fails.
        if let Some(run) = unsafe { heap_ref.acquire_run(class, pages) } {
            let full = {
                // SAFETY: run was just returned by this heap's live arena.
                let run_ref = unsafe { run.as_ref() };
                mag.allocate(run_ref);
                run_ref.is_full()
            };
            if !full {
                // SAFETY: Active TLS owner; `&Run` is not live across this call.
                let _ = unsafe { heap_ref.push_available(run) };
            }
            return;
        }

        // Always flush (empty drain is cheap) then retry — never early-return on a
        // stale empty check while a concurrent Active publish may still land.
        // SAFETY: Active TLS owner.
        if unsafe { heap_ref.flush(pages) }.is_err() {
            return;
        }
        // SAFETY: Active TLS owner.
        if let Some(run) = unsafe { heap_ref.acquire_run(class, pages) } {
            let full = {
                // SAFETY: run was just returned by this heap's live arena.
                let run_ref = unsafe { run.as_ref() };
                mag.allocate(run_ref);
                run_ref.is_full()
            };
            if !full {
                // SAFETY: Active TLS owner; `&Run` is not live across this call.
                let _ = unsafe { heap_ref.push_available(run) };
            }
        }
    }

    /// Cache miss / extent / unbound: `PageMap` then typed free.
    #[cold]
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

    /// Drain one taken magazine through `Heap::free` (no inbox flush).
    #[cold]
    #[inline(never)]
    fn free_magazine(
        &self,
        inner: NonNull<AllocatorInner>,
        mag: Magazine,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: caller is the Active TLS owner; inner is retained while bound.
        let pages = unsafe { inner.as_ref() }.pages();
        let heap = self.bound_heap();
        let mut error = None;
        for ptr in mag {
            let Some(owner) = self.lookup(inner, pages, ptr) else {
                error.get_or_insert(ThreadFreeError::Heap(HeapError::InvalidRunPointer));
                continue;
            };
            let PageOwner::Run(run) = owner else {
                error.get_or_insert(ThreadFreeError::Heap(HeapError::InvalidRunPointer));
                continue;
            };
            // SAFETY: Active TLS owner; `run` is a live PageMap run.
            if let Err(err) = unsafe { heap.as_ref().free_run(run, ptr) } {
                error.get_or_insert(ThreadFreeError::Heap(err));
            }
        }
        match error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn magazine(&self, class: SizeClass) -> &Magazine {
        debug_assert!(class.index() < self.magazines.len());
        // SAFETY: SizeClass values are created only by SizeClasses for indexes in this array.
        unsafe { self.magazines.get_unchecked(class.index()) }
    }

    /// Bound heap pointer after a successful `matches` / heap-id check.
    fn bound_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS entry matched a bound allocator inner.
        unsafe { NonNull::new_unchecked(heap) }
    }

    /// Retire the bound heap and release the inner retain.
    #[cold]
    pub(crate) fn unbind(&self) {
        if let Some(inner) = NonNull::new(self.inner.get()) {
            let mut error = false;
            for mag in &self.magazines {
                if self.free_magazine(inner, mag.take()).is_err() {
                    error = true;
                }
            }
            if error {
                Allocator::abort();
            }
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

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
}
