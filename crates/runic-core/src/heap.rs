use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
};

use crate::{
    allocation::{Allocation, ZeroStatus},
    config::AllocatorConfig,
    extent::{ExtentHeap, ExtentHeapError},
    layout::LayoutSpec,
    local::{HeapRegistry, RemoteFree},
    memory::{PageMap, PageOwner},
    ownership::{HeapId, RunOwner},
    run::{RunHeap, RunHeapError},
    size_class::SizeClasses,
};

pub(crate) struct SharedHeap {
    runs: RunHeap,
    extents: ExtentHeap,
    pages: PageMap,
    heaps: HeapRegistry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    UnknownPointer,
    MissingExtent,
    InvalidRunPointer,
    InvalidExtentPointer,
    DoubleFree,
    InvalidMetadata,
}

impl SharedHeap {
    pub(crate) const DEFAULT_METADATA_CAPACITY: u32 = 65_536;

    pub(crate) const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            runs: RunHeap::new(Self::DEFAULT_METADATA_CAPACITY, config.run()),
            extents: ExtentHeap::new(Self::DEFAULT_METADATA_CAPACITY, config.extent()),
            pages: PageMap::new(),
            heaps: HeapRegistry::new(),
        }
    }

    pub(crate) fn acquire_heap_id(&mut self) -> Option<HeapId> {
        self.heaps.acquire()
    }

    pub(crate) fn refill_local(
        &mut self,
        heap_id: HeapId,
        class: crate::size_class::SizeClassId,
    ) -> Option<(core::ptr::NonNull<crate::run::Run>, Allocation)> {
        let (run, allocation) =
            self.runs
                .allocate_run(class, RunOwner::Thread(heap_id), &mut self.pages)?;

        if !self.heaps.attach_run(heap_id, run) {
            return None;
        }

        Some((run, allocation))
    }

    pub(crate) fn drain_remote(&mut self, heap_id: HeapId) -> Result<(), HeapError> {
        while self.heaps.has_remote(heap_id) {
            let Some(free) = self.heaps.pop_remote(heap_id) else {
                break;
            };
            let run = free.run();
            // SAFETY: remote-free messages are created only from page-map run pointers.
            let run_ref = unsafe { run.as_ref() };
            let was_full = run_ref.is_full();

            run_ref
                .drain_remote_pending(free.ptr())
                .map_err(RunHeapError::from)
                .map_err(HeapError::from)?;

            if run_ref.owner() == RunOwner::Shared && was_full {
                self.runs
                    .push_shared_available(run_ref.class(), run)
                    .map_err(HeapError::from)?;
            }
        }

        Ok(())
    }

    pub(crate) fn retire_local_heap(&mut self, heap_id: HeapId) {
        let _ = self.drain_remote(heap_id);
        let mut owned = self.heaps.release(heap_id);

        while let Some(run) = owned {
            // SAFETY: local owner lists contain stable RunArena pointers.
            let run_ref = unsafe { run.as_ref() };
            owned = run_ref.take_owner_next();

            if run_ref.owner() != RunOwner::Thread(heap_id) {
                continue;
            }

            run_ref.assign_owner(RunOwner::Shared);
            if run_ref.has_available_blocks() {
                let _ = self.runs.push_shared_available(run_ref.class(), run);
            }
        }
    }

    pub(crate) fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);

        self.allocate(spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr())
    }

    pub(crate) fn dealloc(
        &mut self,
        raw_ptr: *mut u8,
        _layout: Layout,
        current_heap: Option<HeapId>,
    ) -> Result<(), HeapError> {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return Ok(());
        };

        let Some(entry) = self.pages.get(ptr) else {
            return Err(HeapError::UnknownPointer);
        };

        match entry {
            PageOwner::Run(run) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
                let run_ref = unsafe { run.as_ref() };

                match run_ref.owner() {
                    RunOwner::Shared => self
                        .runs
                        .free(run, ptr, &mut self.pages)
                        .map_err(HeapError::from),
                    RunOwner::Thread(heap_id) => {
                        if Some(heap_id) == current_heap {
                            self.drain_remote(heap_id)?;
                            return run_ref
                                .free(ptr)
                                .map_err(RunHeapError::from)
                                .map_err(HeapError::from);
                        }

                        self.enqueue_remote_free(heap_id, run, ptr)
                    }
                }
            }
            PageOwner::Extent(extent) => self
                .extents
                .free(extent, ptr, &mut self.pages)
                .map_err(HeapError::from),
        }
    }

    fn enqueue_remote_free(
        &mut self,
        heap_id: HeapId,
        run: core::ptr::NonNull<crate::run::Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        let run_ref = unsafe { run.as_ref() };
        run_ref
            .mark_remote_pending(ptr)
            .map_err(RunHeapError::from)
            .map_err(HeapError::from)?;

        let free = RemoteFree::new(run, ptr);
        if self.heaps.enqueue_remote(heap_id, free).is_ok() {
            return Ok(());
        }

        self.drain_remote(heap_id)?;

        self.heaps
            .enqueue_remote(heap_id, free)
            .map_err(|_| HeapError::InvalidMetadata)
    }

    pub(crate) fn realloc(
        &mut self,
        ptr: *mut u8,
        old: Layout,
        new_size: usize,
    ) -> Result<*mut u8, HeapError> {
        if ptr.is_null() {
            let Some(spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
                return Ok(null_mut());
            };

            return Ok(self
                .allocate(spec)
                .map_or(null_mut(), |allocation| allocation.ptr().as_ptr()));
        }

        if new_size == 0 {
            self.dealloc(ptr, old, None)?;
            return Ok(null_mut());
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return Ok(null_mut());
        };

        let Some(entry) = self.pages.get(old_ptr) else {
            return Err(HeapError::UnknownPointer);
        };

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return Ok(null_mut());
        };
        let new_spec = LayoutSpec::from_layout(new_layout);

        match entry {
            PageOwner::Run(run) => {
                if RunHeap::resize_in_place(run, old_ptr, new_spec).map_err(HeapError::from)? {
                    return Ok(ptr);
                }
            }
            PageOwner::Extent(extent) => {
                if ExtentHeap::resize_in_place(extent, old_ptr, new_spec)
                    .map_err(HeapError::from)?
                {
                    return Ok(ptr);
                }
            }
        }

        let new_ptr = self
            .allocate(new_spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr());

        if new_ptr.is_null() {
            return Ok(null_mut());
        }

        // SAFETY: new_ptr is a fresh allocation of at least new_layout.size() bytes; ptr is valid for old.size().
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_layout.size())) };

        if let Err(error) = self.dealloc(ptr, old, None) {
            let _ = self.dealloc(new_ptr, new_layout, None);

            return Err(error);
        }

        Ok(new_ptr)
    }

    pub(crate) fn alloc_zeroed(&mut self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let Some(allocation) = self.allocate(spec) else {
            return null_mut();
        };
        let ptr = allocation.ptr().as_ptr();

        if allocation.zero_status() == ZeroStatus::NeedsZeroing {
            // SAFETY: ptr is valid for layout.size() bytes because it was just allocated for layout.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }

        ptr
    }

    fn allocate(&mut self, spec: LayoutSpec) -> Option<Allocation> {
        match SizeClasses::id_for(spec) {
            Some(class) => self.runs.allocate(class, RunOwner::Shared, &mut self.pages),
            None => self.extents.allocate(spec, &mut self.pages),
        }
    }
}

impl From<RunHeapError> for HeapError {
    fn from(error: RunHeapError) -> Self {
        match error {
            RunHeapError::InvalidPointer => Self::InvalidRunPointer,
            RunHeapError::DoubleFree => Self::DoubleFree,
            RunHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<ExtentHeapError> for HeapError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent => Self::MissingExtent,
            ExtentHeapError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{config::ExtentReuse, ownership::RunOwner};

    use super::*;

    fn test_heap() -> SharedHeap {
        test_heap_with_config(AllocatorConfig::new())
    }

    fn test_heap_with_config(config: AllocatorConfig) -> SharedHeap {
        SharedHeap {
            runs: RunHeap::new(4, config.run()),
            extents: ExtentHeap::new(4, config.extent()),
            pages: PageMap::new(),
            heaps: HeapRegistry::new(),
        }
    }

    #[test]
    fn heap_reports_small_double_free() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));
        assert_eq!(heap.dealloc(ptr, layout, None), Err(HeapError::DoubleFree));
    }

    #[test]
    fn heap_reports_small_realloc_after_free() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));
        assert_eq!(heap.realloc(ptr, layout, 128), Err(HeapError::DoubleFree));
    }

    #[test]
    fn heap_reuses_freed_large_extent_mapping() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let first = heap.alloc(layout);

        assert!(!first.is_null());
        assert_eq!(heap.dealloc(first, layout, None), Ok(()));

        let second = heap.alloc(layout);
        assert_eq!(second, first);
        assert_eq!(heap.dealloc(second, layout, None), Ok(()));
    }

    #[test]
    fn heap_realloc_grows_reused_extent_within_cached_mapping() {
        let config = AllocatorConfig::new().with_extent_reuse(ExtentReuse::BestFit);
        let mut heap = test_heap_with_config(config);
        let large = Layout::from_size_align(512 * 1024, 4096).unwrap();
        let small = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let grown_layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let large_ptr = heap.alloc(large);

        assert!(!large_ptr.is_null());
        assert_eq!(heap.dealloc(large_ptr, large, None), Ok(()));

        let small_ptr = heap.alloc(small);
        assert_eq!(small_ptr, large_ptr);

        let grown = heap.realloc(small_ptr, small, 256 * 1024).unwrap();
        assert_eq!(grown, small_ptr);
        assert_eq!(heap.dealloc(grown, grown_layout, None), Ok(()));
    }

    #[test]
    fn heap_reports_large_double_free_as_unknown_pointer_after_caching() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));
        assert_eq!(
            heap.dealloc(ptr, layout, None),
            Err(HeapError::UnknownPointer)
        );
    }

    #[test]
    fn heap_reports_large_realloc_after_free_as_unknown_pointer_after_caching() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));
        assert_eq!(
            heap.realloc(ptr, layout, 512 * 1024),
            Err(HeapError::UnknownPointer)
        );
    }

    #[test]
    fn heap_realloc_keeps_same_run_block_for_same_size_class() {
        let mut heap = test_heap();
        let old = Layout::from_size_align(49, 8).unwrap();
        let ptr = heap.alloc(old);

        assert!(!ptr.is_null());
        for index in 0..old.size() {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: ptr was allocated for old.size() bytes above.
            unsafe { ptr.add(index).write(value) };
        }

        let new_ptr = heap.realloc(ptr, old, 64).unwrap();

        assert_eq!(new_ptr, ptr);
        for index in 0..old.size() {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: new_ptr is the same live allocation and old.size() bytes remain initialized.
            assert_eq!(unsafe { new_ptr.add(index).read() }, value);
        }

        let new = Layout::from_size_align(64, 8).unwrap();
        assert_eq!(heap.dealloc(new_ptr, new, None), Ok(()));
    }

    #[test]
    fn heap_realloc_keeps_extent_when_new_layout_fits() {
        let mut heap = test_heap();
        let old = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(old);

        assert!(!ptr.is_null());
        for index in 0..4096 {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: ptr was allocated for old.size() bytes above and index is within that range.
            unsafe { ptr.add(index).write(value) };
        }

        let new_ptr = heap.realloc(ptr, old, 128 * 1024).unwrap();

        assert_eq!(new_ptr, ptr);
        for index in 0..4096 {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: new_ptr is the same live allocation and these prefix bytes remain initialized.
            assert_eq!(unsafe { new_ptr.add(index).read() }, value);
        }

        let new = Layout::from_size_align(128 * 1024, 4096).unwrap();
        assert_eq!(heap.dealloc(new_ptr, new, None), Ok(()));
    }

    #[test]
    fn heap_realloc_grows_extent_within_published_page_range() {
        let mut heap = test_heap();
        let old = Layout::from_size_align(64 * 1024 - 1, 8).unwrap();
        let ptr = heap.alloc(old);

        assert!(!ptr.is_null());
        for index in 0..4096 {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: ptr was allocated for old.size() bytes above and index is within that range.
            unsafe { ptr.add(index).write(value) };
        }

        let new_ptr = heap.realloc(ptr, old, 64 * 1024).unwrap();

        assert_eq!(new_ptr, ptr);
        for index in 0..4096 {
            let value = u8::try_from(index % 251).unwrap().wrapping_add(1);
            // SAFETY: new_ptr is the same live allocation and these prefix bytes remain initialized.
            assert_eq!(unsafe { new_ptr.add(index).read() }, value);
        }

        let new = Layout::from_size_align(64 * 1024, 8).unwrap();
        assert_eq!(heap.dealloc(new_ptr, new, None), Ok(()));
    }

    #[test]
    fn heap_zeroes_reused_run_block() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated for layout.size() bytes above.
        unsafe { write_bytes(ptr, 0xab, layout.size()) };
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));

        let zeroed = heap.alloc_zeroed(layout);
        assert!(!zeroed.is_null());
        // SAFETY: zeroed was allocated for layout.size() bytes above.
        let bytes = unsafe { core::slice::from_raw_parts(zeroed, layout.size()) };
        assert!(bytes.iter().all(|&byte| byte == 0));

        assert_eq!(heap.dealloc(zeroed, layout, None), Ok(()));
    }

    #[test]
    fn heap_zeroes_reused_extent_mapping() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated for layout.size() bytes above.
        unsafe { write_bytes(ptr, 0xab, layout.size()) };
        assert_eq!(heap.dealloc(ptr, layout, None), Ok(()));

        let zeroed = heap.alloc_zeroed(layout);
        assert_eq!(zeroed, ptr);
        // SAFETY: zeroed was allocated for layout.size() bytes above.
        let bytes = unsafe { core::slice::from_raw_parts(zeroed, layout.size()) };
        assert!(bytes.iter().all(|&byte| byte == 0));

        assert_eq!(heap.dealloc(zeroed, layout, None), Ok(()));
    }

    #[test]
    fn heap_routes_remote_free_to_owner_inbox() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::id_for(LayoutSpec::from_layout(layout)).unwrap();
        let heap_id = heap.acquire_heap_id().unwrap();
        let (run, allocation) = heap.refill_local(heap_id, class).unwrap();
        let ptr = allocation.ptr();

        assert_eq!(heap.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(
            heap.dealloc(ptr.as_ptr(), layout, None),
            Err(HeapError::DoubleFree)
        );

        assert_eq!(heap.drain_remote(heap_id), Ok(()));
        // SAFETY: refill_local returns a stable RunArena pointer that remains published.
        assert_eq!(unsafe { run.as_ref() }.owner(), RunOwner::Thread(heap_id));
    }

    #[test]
    fn heap_remote_drain_returns_block_to_owner_run() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::id_for(LayoutSpec::from_layout(layout)).unwrap();
        let heap_id = heap.acquire_heap_id().unwrap();
        let (run, allocation) = heap.refill_local(heap_id, class).unwrap();
        let ptr = allocation.ptr();

        assert_eq!(heap.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(heap.drain_remote(heap_id), Ok(()));

        // SAFETY: refill_local returns a stable RunArena pointer that remains published.
        let run_ref = unsafe { run.as_ref() };
        assert_eq!(run_ref.allocate().map(crate::run::RunBlock::ptr), Some(ptr));
    }

    #[test]
    fn heap_remote_queue_full_retry_does_not_drop_free() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::id_for(LayoutSpec::from_layout(layout)).unwrap();
        let heap_id = heap.acquire_heap_id().unwrap();
        let (run, first) = heap.refill_local(heap_id, class).unwrap();
        let mut ptrs = [first.ptr(); 17];

        for ptr in ptrs.iter_mut().skip(1) {
            // SAFETY: refill_local returns a stable RunArena pointer that remains published.
            *ptr = unsafe { run.as_ref() }.allocate().unwrap().ptr();
        }

        for ptr in ptrs {
            assert_eq!(heap.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        }

        assert_eq!(heap.drain_remote(heap_id), Ok(()));
    }

    #[test]
    fn heap_retirement_transfers_thread_runs_to_shared() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::id_for(LayoutSpec::from_layout(layout)).unwrap();
        let heap_id = heap.acquire_heap_id().unwrap();
        let (run, allocation) = heap.refill_local(heap_id, class).unwrap();
        let ptr = allocation.ptr();

        heap.retire_local_heap(heap_id);

        // SAFETY: refill_local returns a stable RunArena pointer that remains published.
        assert_eq!(unsafe { run.as_ref() }.owner(), RunOwner::Shared);
        assert_eq!(heap.dealloc(ptr.as_ptr(), layout, None), Ok(()));
    }
}
