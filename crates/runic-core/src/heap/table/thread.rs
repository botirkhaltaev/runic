use core::{
    cell::{Cell, UnsafeCell},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    allocator::{Allocator, AllocatorInner},
    heap::{Extent, ExtentInit, Heap, HeapId, HeapTable, Run, RunError},
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::{SizeClassId, SizeClasses},
};

use super::{inbox::RemoteList, slot::HeapError};

/// Owner-local TLS free failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadFreeError {
    /// Unbound or bound to a different heap — caller takes `free_remote`.
    NotBound,
    Heap(HeapError),
}

const REMOTE_BATCH_CAPACITY: u32 = 32;

/// Producer-side coalesce buffer for remote frees.
struct RemoteBatch {
    target: Option<HeapId>,
    first: Option<NonNull<u8>>,
    last: Option<NonNull<u8>>,
    len: u32,
}

impl RemoteBatch {
    const fn new() -> Self {
        Self {
            target: None,
            first: None,
            last: None,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append `ptr` for `target`.
    ///
    /// Returns a list that must be published to the returned `HeapId`'s inbox when the
    /// previous batch is full or the target changes.
    fn append(&mut self, target: HeapId, ptr: NonNull<u8>) -> Option<(HeapId, RemoteList)> {
        let pending = if self.target.is_some_and(|current| current != target) {
            self.take()
        } else {
            None
        };

        self.target = Some(target);
        Self::store_next(ptr, core::ptr::null_mut());

        if let Some(last) = self.last {
            Self::store_next(last, ptr.as_ptr());
            self.last = Some(ptr);
        } else {
            self.first = Some(ptr);
            self.last = Some(ptr);
        }

        self.len += 1;
        if self.len >= REMOTE_BATCH_CAPACITY {
            // After a target-change take, len is 1, so this cannot coincide with `pending`.
            debug_assert!(pending.is_none());
            return self.take();
        }

        pending
    }

    /// Take any pending nodes for publish (partial flush).
    fn take(&mut self) -> Option<(HeapId, RemoteList)> {
        let target = self.target?;
        let first = self.first?;
        let last = self.last?;
        self.clear();
        Some((target, RemoteList::from_ends(first, last)))
    }

    fn clear(&mut self) {
        self.target = None;
        self.first = None;
        self.last = None;
        self.len = 0;
    }

    fn store_next(ptr: NonNull<u8>, next: *mut u8) {
        // SAFETY: remote-pending blocks store the intrusive next at the base address.
        unsafe {
            (*ptr.as_ptr().cast::<AtomicPtr<u8>>()).store(next, Ordering::Relaxed);
        }
    }
}

/// Thread-local frontend: bound heap, cached runs, and outbound remote batch.
pub(crate) struct ThreadHeap {
    inner: Cell<*mut AllocatorInner>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    runs: [Cell<*mut Run>; SizeClasses::COUNT],
    /// Last cached run page number (`usize::MAX` = empty). See `lookup_owner`.
    page_cache_page: Cell<usize>,
    page_cache_owner: Cell<Option<PageOwner>>,
    remote: UnsafeCell<RemoteBatch>,
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
            remote: UnsafeCell::new(RemoteBatch::new()),
        }
    }

    /// `PageMap` lookup with a one-entry TLS page→run cache (miss fills from `pages`).
    ///
    /// Only **run** owners are cached: runs stay published for the heap lifetime in
    /// v0.5. Extents may `unpublish` / reuse VA, so caching them would go stale.
    #[inline]
    pub(crate) fn lookup_owner(&self, pages: &PageMap, ptr: NonNull<u8>) -> Option<PageOwner> {
        let page = ptr.as_ptr().addr() / crate::memory::PAGE_SIZE;
        if self.page_cache_page.get() == page {
            return self.page_cache_owner.get();
        }

        let owner = pages.get(ptr)?;
        if matches!(owner, PageOwner::Run(_)) {
            self.page_cache_page.set(page);
            self.page_cache_owner.set(Some(owner));
        }
        Some(owner)
    }

    fn clear_page_cache(&self) {
        self.page_cache_page.set(usize::MAX);
        self.page_cache_owner.set(None);
    }

    /// Owner-local small allocation via the TLS run cache.
    ///
    /// Returns `None` when this thread is not bound to `inner` (caller should `bind`).
    /// Sticky hit is the straight-line body; inbox flush / acquire is cold.
    pub(crate) fn alloc(
        &self,
        inner: NonNull<AllocatorInner>,
        class: SizeClassId,
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

        self.alloc_miss(class, pages, self.bound_heap())
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

        // SAFETY: heap is bound only while this TLS entry retains the allocator inner.
        unsafe { self.bound_heap().as_mut() }.allocate_extent(spec, pages, init)
    }

    /// Owner-local free for a run owned by the bound heap.
    ///
    /// Sticky hit is the straight-line body (unbind clears sticky ⇒ sticky implies bound);
    /// non-cached owner free goes through `Heap::free` after `matches` / `HeapId`.
    #[inline]
    pub(crate) fn free(
        &self,
        inner: NonNull<AllocatorInner>,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_ref() };
        let class = run_ref.class();
        // Sticky before matches: slots only park this heap's runs and are cleared on unbind.
        if self.run_cell(class).get() == run.as_ptr() {
            // Sticky hit: Run only — do not finish_free / push_available.
            return match run_ref.free(ptr) {
                Ok(()) => Ok(()),
                Err(error) => Err(Self::sticky_free_err(error)),
            };
        }

        if !self.matches(inner) || self.heap_id.get() != Some(run_ref.heap_id()) {
            return Err(ThreadFreeError::NotBound);
        }

        // SAFETY: heap is bound only while this TLS entry retains the allocator inner.
        unsafe { self.bound_heap().as_mut() }
            .free(
                PageOwner::Run(run),
                ptr,
                // SAFETY: inner is retained by this TLS entry while bound.
                unsafe { inner.as_ref().pages() },
            )
            .map_err(ThreadFreeError::Heap)
    }

    #[cold]
    #[inline(never)]
    fn sticky_free_err(error: RunError) -> ThreadFreeError {
        ThreadFreeError::Heap(error.into())
    }

    /// Owner-local free for an extent owned by the bound heap.
    pub(crate) fn free_extent(
        &self,
        inner: NonNull<AllocatorInner>,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
    ) -> Result<(), ThreadFreeError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        if !self.matches(inner) || self.heap_id.get() != Some(heap_id) {
            return Err(ThreadFreeError::NotBound);
        }

        // SAFETY: heap is bound only while this TLS entry retains the allocator inner.
        unsafe { self.bound_heap().as_mut() }
            .free(
                PageOwner::Extent(extent),
                ptr,
                // SAFETY: inner is retained by this TLS entry while bound.
                unsafe { inner.as_ref().pages() },
            )
            .map_err(ThreadFreeError::Heap)
    }

    /// Bound heap id when this TLS entry is attached to `inner`.
    pub(crate) fn bound(&self, inner: NonNull<AllocatorInner>) -> Option<HeapId> {
        self.matches(inner).then_some(())?;
        self.heap_id.get()
    }

    /// Bind this thread to a heap in `inner`'s table.
    ///
    /// Reuses the current binding when already attached to `inner`; otherwise unbinds any
    /// foreign binding and acquires a fresh heap under the caller's table lock.
    pub(crate) fn bind(
        &self,
        inner: NonNull<AllocatorInner>,
        table: &mut HeapTable,
    ) -> Option<HeapId> {
        if self.matches(inner) {
            return self.heap_id.get();
        }

        if !self.is_empty() {
            self.unbind();
        }

        if !AllocatorInner::retain(inner) {
            return None;
        }

        let Some((id, heap)) = table.acquire() else {
            AllocatorInner::release(inner);
            return None;
        };
        self.install(inner, heap, id);

        Some(id)
    }

    /// Coalesce a claimed remote free; returns a list the caller must `HeapTable::publish`.
    pub(crate) fn batch(&self, target: HeapId, ptr: NonNull<u8>) -> Option<(HeapId, RemoteList)> {
        // SAFETY: ThreadHeap is thread-local; exclusive access to the remote batch.
        unsafe { &mut *self.remote.get() }.append(target, ptr)
    }

    /// Take any pending outbound remote frees (unbind / protocol flush).
    pub(crate) fn take_batch(&self) -> Option<(HeapId, RemoteList)> {
        // SAFETY: ThreadHeap is thread-local; exclusive access to the remote batch.
        let batch = unsafe { &mut *self.remote.get() };
        if batch.is_empty() {
            return None;
        }
        batch.take()
    }

    fn install(&self, inner: NonNull<AllocatorInner>, heap: NonNull<Heap>, id: HeapId) {
        self.heap.set(heap.as_ptr());
        self.heap_id.set(Some(id));
        self.inner.set(inner.as_ptr());
    }

    fn matches(&self, inner: NonNull<AllocatorInner>) -> bool {
        self.inner.get() == inner.as_ptr()
    }

    fn is_empty(&self) -> bool {
        self.inner.get().is_null()
    }

    #[cold]
    fn alloc_miss(
        &self,
        class: SizeClassId,
        pages: &PageMap,
        mut heap: NonNull<Heap>,
    ) -> Option<NonNull<u8>> {
        let cell = self.run_cell(class);

        // SAFETY: heap is bound only while this TLS entry retains the allocator inner.
        let heap_mut = unsafe { heap.as_mut() };
        if !heap_mut.inbox().is_empty() {
            heap_mut.flush(pages).ok()?;
            if let Some(run) = NonNull::new(cell.get()) {
                // SAFETY: sticky run pointers are published from this heap's live arena.
                if let Some(ptr) = unsafe { run.as_ref() }.allocate() {
                    return Some(ptr);
                }
                cell.set(core::ptr::null_mut());
            }
        }

        // Inbox is empty after the optional flush above, so acquire_run will not flush again.
        let run = heap_mut.acquire_run(class, pages)?;
        cell.set(run.as_ptr());
        // SAFETY: run was just returned by this heap's live arena.
        unsafe { run.as_ref() }.allocate()
    }

    fn run_cell(&self, class: SizeClassId) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.runs.len());
        // SAFETY: SizeClassId values are created only by SizeClasses for indexes in this array.
        unsafe { self.runs.get_unchecked(class.index()) }
    }

    /// Bound heap pointer after a successful `matches` / heap-id check.
    fn bound_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS entry matched a bound allocator inner.
        unsafe { NonNull::new_unchecked(heap) }
    }

    fn return_cached_runs(&self) {
        let Some(mut heap) = NonNull::new(self.heap.get()) else {
            return;
        };

        for run in &self.runs {
            let Some(run) = NonNull::new(run.replace(core::ptr::null_mut())) else {
                continue;
            };

            // SAFETY: heap is bound only while this TLS entry retains the allocator inner.
            let _ = unsafe { heap.as_mut() }.runs.return_available(run);
        }
    }

    fn publish_batches(&self, inner: &AllocatorInner) {
        while let Some((id, list)) = self.take_batch() {
            let mut table = inner.table.lock();
            if table.publish(id, &list, inner.pages()).is_err() {
                Allocator::abort();
            }
        }
    }

    #[cold]
    fn unbind(&self) {
        self.return_cached_runs();
        self.clear_page_cache();
        let Some(inner) = NonNull::new(self.inner.replace(core::ptr::null_mut())) else {
            return;
        };
        let heap_id = self.heap_id.replace(None);
        self.heap.set(core::ptr::null_mut());

        // SAFETY: this TLS entry retained inner while bound.
        let inner_ref = unsafe { inner.as_ref() };
        self.publish_batches(inner_ref);

        if let Some(heap_id) = heap_id {
            let mut table = inner_ref.table.lock();
            if table.retire(heap_id, inner_ref.pages()).is_err() {
                Allocator::abort();
            }
        }

        AllocatorInner::release(inner);
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU32;

    use super::*;
    use crate::heap::table::inbox::Inbox;

    #[repr(C)]
    struct TestNode {
        next: AtomicPtr<u8>,
    }

    fn node_ptr(node: &TestNode) -> NonNull<u8> {
        NonNull::new(core::ptr::from_ref(node).cast::<u8>().cast_mut()).unwrap()
    }

    fn heap_id(index: u32) -> HeapId {
        HeapId::new(index, NonZeroU32::MIN).unwrap()
    }

    #[test]
    fn remote_batch_publishes_at_capacity() {
        let inbox = Inbox::new();

        let mut nodes = [const {
            TestNode {
                next: AtomicPtr::new(core::ptr::null_mut()),
            }
        }; REMOTE_BATCH_CAPACITY as usize];
        let mut batch = RemoteBatch::new();
        let target = heap_id(0);

        let mut published = None;
        for node in &mut nodes {
            if let Some(list) = batch.append(target, node_ptr(node)) {
                published = Some(list);
            }
        }

        assert!(batch.is_empty());
        let (id, list) = published.unwrap();
        assert_eq!(id, target);
        inbox.push_batch(&list);
        assert_eq!(
            inbox.drain().unwrap().count(),
            REMOTE_BATCH_CAPACITY as usize
        );
    }

    #[test]
    fn remote_batch_returns_list_on_target_change() {
        let a = TestNode {
            next: AtomicPtr::new(core::ptr::null_mut()),
        };
        let b = TestNode {
            next: AtomicPtr::new(core::ptr::null_mut()),
        };
        let mut batch = RemoteBatch::new();
        assert!(batch.append(heap_id(0), node_ptr(&a)).is_none());

        let (id, list) = batch.append(heap_id(1), node_ptr(&b)).unwrap();
        assert_eq!(id, heap_id(0));
        assert_eq!(list.first, node_ptr(&a));
        assert_eq!(list.last, node_ptr(&a));
        assert!(!batch.is_empty());
    }
}
