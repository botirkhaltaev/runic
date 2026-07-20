use core::{
    cell::{Cell, UnsafeCell},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    allocator::{AllocatorCore, AllocatorState},
    heap::{Heap, HeapId, Run, RunHeapError},
    memory::PageMap,
    size_class::{SizeClassId, SizeClasses},
};

use super::{inbox::RemoteList, slot::HeapError};

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

pub(crate) struct ThreadHeap {
    core: Cell<*mut AllocatorCore>,
    heap_id: Cell<Option<HeapId>>,
    heap: Cell<*mut Heap>,
    runs: [Cell<*mut Run>; SizeClasses::COUNT],
    remote: UnsafeCell<RemoteBatch>,
}

impl Drop for ThreadHeap {
    fn drop(&mut self) {
        self.release_current();
    }
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            core: Cell::new(core::ptr::null_mut()),
            heap_id: Cell::new(None),
            heap: Cell::new(core::ptr::null_mut()),
            runs: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
            remote: UnsafeCell::new(RemoteBatch::new()),
        }
    }

    pub(crate) fn allocate_run(
        &self,
        core: NonNull<AllocatorCore>,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.matches(core) {
            return None;
        }

        self.allocate_run_current(class, pages)
    }

    pub(crate) fn free_run(
        &self,
        core: NonNull<AllocatorCore>,
        heap: HeapId,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Option<Result<(), HeapError>> {
        if !self.matches(core) || self.heap_id.get()? != heap {
            return None;
        }

        Some(self.free_run_current(
            run,
            ptr,
            // SAFETY: core is retained by the calling TLS heap while installed.
            unsafe { core.as_ref().pages() },
        ))
    }

    pub(crate) fn heap_id(&self, core: NonNull<AllocatorCore>) -> Option<HeapId> {
        self.matches(core).then_some(())?;
        self.heap_id.get()
    }

    pub(crate) fn release_if_different(&self, core: NonNull<AllocatorCore>) {
        if !self.is_empty() && !self.matches(core) {
            self.release_current();
        }
    }

    pub(crate) fn get_or_acquire(
        &self,
        core: NonNull<AllocatorCore>,
        state: &mut AllocatorState,
    ) -> Option<HeapId> {
        if self.matches(core) {
            return self.heap_id.get();
        }

        debug_assert!(self.is_empty());
        if !AllocatorCore::retain(core) {
            return None;
        }

        let Some(heap) = state.acquire_heap() else {
            AllocatorCore::release(core);
            return None;
        };
        // SAFETY: acquire returns a live table-resident heap.
        let id = unsafe { heap.as_ref().id() };
        self.install(core, heap, id);

        Some(id)
    }

    /// Coalesce a remote free; returns a list the caller must publish when present.
    pub(crate) fn enqueue_remote(
        &self,
        target: HeapId,
        ptr: NonNull<u8>,
    ) -> Option<(HeapId, RemoteList)> {
        // SAFETY: ThreadHeap is thread-local; exclusive access to the remote batch.
        unsafe { &mut *self.remote.get() }.append(target, ptr)
    }

    /// Take any pending outbound remote frees (TLS release / protocol flush).
    pub(crate) fn take_remote(&self) -> Option<(HeapId, RemoteList)> {
        // SAFETY: ThreadHeap is thread-local; exclusive access to the remote batch.
        let batch = unsafe { &mut *self.remote.get() };
        if batch.is_empty() {
            return None;
        }
        batch.take()
    }

    fn install(&self, core: NonNull<AllocatorCore>, heap: NonNull<Heap>, id: HeapId) {
        self.heap.set(heap.as_ptr());
        self.heap_id.set(Some(id));
        self.core.set(core.as_ptr());
    }

    fn matches(&self, core: NonNull<AllocatorCore>) -> bool {
        self.core.get() == core.as_ptr()
    }

    fn is_empty(&self) -> bool {
        self.core.get().is_null()
    }

    fn heap_ptr(&self) -> Option<NonNull<Heap>> {
        NonNull::new(self.heap.get())
    }

    fn allocate_run_current(&self, class: SizeClassId, pages: &PageMap) -> Option<NonNull<u8>> {
        let mut heap = self.installed_heap();

        if let Some(allocation) = self.allocate_cached_run(class, heap) {
            return Some(allocation);
        }

        // SAFETY: heap is installed only while this TLS heap retains the allocator core.
        if unsafe { heap.as_mut() }.flush(pages).is_ok()
            && let Some(allocation) = self.allocate_cached_run(class, heap)
        {
            return Some(allocation);
        }

        // SAFETY: heap is installed only while this TLS heap retains the allocator core.
        let heap_mut = unsafe { heap.as_mut() };
        heap_mut.flush(pages).ok()?;
        let run = heap_mut.take_or_allocate_run(class, pages)?;
        self.cache_run(class, run);

        self.allocate_cached_run(class, heap)
    }

    fn allocate_cached_run(
        &self,
        class: SizeClassId,
        mut heap: NonNull<Heap>,
    ) -> Option<NonNull<u8>> {
        let mut run = self.cached_run(class)?;

        // SAFETY: heap is installed only while this TLS heap retains the allocator core.
        let heap = unsafe { heap.as_mut() };
        // SAFETY: cached run pointers are published from this heap's live arena.
        let Some(allocation) = unsafe { run.as_mut() }.allocate() else {
            self.clear_run(class);
            return None;
        };
        heap.retain_allocation();
        Some(allocation)
    }

    fn free_run_current(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        let mut heap = self.installed_heap();
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let class = unsafe { run.as_ref() }.class();

        if self.cached_run(class) == Some(run) {
            // SAFETY: heap is installed only while this TLS heap retains the allocator core.
            let heap = unsafe { heap.as_mut() };
            // SAFETY: cached run pointers are published from this heap's live arena.
            unsafe { run.as_ref() }
                .free_local(ptr)
                .map_err(RunHeapError::from)
                .map_err(HeapError::from)?;
            heap.release_allocation();
            return Ok(());
        }

        // SAFETY: this TLS thread is the Active owner of the installed heap.
        let heap_mut = unsafe { heap.as_mut() };
        if !heap_mut.inbox().is_empty() {
            heap_mut.flush(pages)?;
        }
        heap_mut.free_run(run, ptr).map_err(HeapError::from)
    }

    fn cached_run(&self, class: SizeClassId) -> Option<NonNull<Run>> {
        NonNull::new(self.run_cell(class).get())
    }

    fn cache_run(&self, class: SizeClassId, run: NonNull<Run>) {
        self.run_cell(class).set(run.as_ptr());
    }

    fn clear_run(&self, class: SizeClassId) {
        self.run_cell(class).set(core::ptr::null_mut());
    }

    fn run_cell(&self, class: SizeClassId) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.runs.len());
        // SAFETY: SizeClassId values are created only by SizeClasses for indexes in this array.
        unsafe { self.runs.get_unchecked(class.index()) }
    }

    fn installed_heap(&self) -> NonNull<Heap> {
        let heap = self.heap.get();
        debug_assert!(!heap.is_null());
        // SAFETY: callers reach this only after this TLS heap matched an installed allocator core.
        unsafe { NonNull::new_unchecked(heap) }
    }

    fn return_cached_runs(&self) {
        let Some(mut heap) = self.heap_ptr() else {
            return;
        };

        for run in &self.runs {
            let Some(run) = NonNull::new(run.replace(core::ptr::null_mut())) else {
                continue;
            };

            // SAFETY: heap is installed only while this TLS heap retains the allocator core.
            let _ = unsafe { heap.as_mut() }.runs.return_available(run);
        }
    }

    fn publish_outbound(&self, core_ref: &AllocatorCore) {
        while let Some((id, list)) = self.take_remote() {
            let state = core_ref.state().lock();
            if state.heaps.push_remote_batch(id, &list).is_err() {
                abort();
            }
        }
    }

    #[cold]
    fn release_current(&self) {
        self.return_cached_runs();
        let Some(core) = NonNull::new(self.core.replace(core::ptr::null_mut())) else {
            return;
        };
        let heap_id = self.heap_id.replace(None);
        self.heap.set(core::ptr::null_mut());

        // SAFETY: this TLS heap retained core while installed.
        let core_ref = unsafe { core.as_ref() };
        self.publish_outbound(core_ref);

        if let Some(heap_id) = heap_id {
            let mut state = core_ref.state().lock();
            if state.release_heap(heap_id, core_ref.pages()).is_err() {
                abort();
            }
        }

        AllocatorCore::release(core);
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
}

#[cold]
#[inline(never)]
fn abort() -> ! {
    // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
    unsafe { libc::abort() }
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
        assert_eq!(list.first, Some(node_ptr(&a)));
        assert_eq!(list.last, node_ptr(&a));
        assert!(!batch.is_empty());
    }
}
