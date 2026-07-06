use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
};

use crate::{
    allocation::{Allocation, ZeroStatus},
    extent::{ExtentAllocator, ExtentAllocatorError},
    layout::LayoutSpec,
    memory::{PageEntry, PageMap},
    run::{RunAllocator, RunAllocatorError},
    size_class::SizeClasses,
};

pub(crate) struct Heap {
    runs: RunAllocator,
    extents: ExtentAllocator,
    pages: PageMap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    UnknownPointer,
    MissingRun,
    MissingExtent,
    InvalidRunPointer,
    InvalidExtentPointer,
    DoubleFree,
    InvalidMetadata,
}

impl Heap {
    pub(crate) const DEFAULT_TABLE_CAPACITY: u32 = 65_536;

    pub(crate) const fn new() -> Self {
        Self {
            runs: RunAllocator::new(Self::DEFAULT_TABLE_CAPACITY),
            extents: ExtentAllocator::new(Self::DEFAULT_TABLE_CAPACITY),
            pages: PageMap::new(),
        }
    }

    pub(crate) fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);

        self.allocate(spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr())
    }

    pub(crate) fn dealloc(&mut self, raw_ptr: *mut u8, _layout: Layout) -> Result<(), HeapError> {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return Ok(());
        };

        let Some(entry) = self.pages.get(ptr) else {
            return Err(HeapError::UnknownPointer);
        };

        match entry {
            PageEntry::Run(id) => self.runs.free(id, ptr).map_err(HeapError::from),
            PageEntry::Extent(id) => self
                .extents
                .free(id, ptr, &mut self.pages)
                .map_err(HeapError::from),
        }
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
            self.dealloc(ptr, old)?;
            return Ok(null_mut());
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return Ok(null_mut());
        };

        let Some(entry) = self.pages.get(old_ptr) else {
            return Err(HeapError::UnknownPointer);
        };
        match entry {
            PageEntry::Run(id) => self
                .runs
                .validate_allocated(id, old_ptr)
                .map_err(HeapError::from)?,
            PageEntry::Extent(id) => self
                .extents
                .validate_allocated(id, old_ptr)
                .map_err(HeapError::from)?,
        }

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return Ok(null_mut());
        };
        let new_spec = LayoutSpec::from_layout(new_layout);
        let new_ptr = self
            .allocate(new_spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr());

        if new_ptr.is_null() {
            return Ok(null_mut());
        }

        // SAFETY: new_ptr is a fresh allocation of at least new_size bytes; ptr is valid for old.size().
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_size)) };

        if let Err(error) = self.dealloc(ptr, old) {
            let _ = self.dealloc(new_ptr, new_layout);

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
        match SizeClasses::get(spec) {
            Some(class) => self.runs.allocate(spec, class, &mut self.pages),
            None => self.extents.allocate(spec, &mut self.pages),
        }
    }
}

impl From<RunAllocatorError> for HeapError {
    fn from(error: RunAllocatorError) -> Self {
        match error {
            RunAllocatorError::MissingRun => Self::MissingRun,
            RunAllocatorError::InvalidPointer => Self::InvalidRunPointer,
            RunAllocatorError::DoubleFree => Self::DoubleFree,
            RunAllocatorError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<ExtentAllocatorError> for HeapError {
    fn from(error: ExtentAllocatorError) -> Self {
        match error {
            ExtentAllocatorError::MissingExtent => Self::MissingExtent,
            ExtentAllocatorError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentAllocatorError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_heap() -> Heap {
        Heap {
            runs: RunAllocator::new(4),
            extents: ExtentAllocator::new(4),
            pages: PageMap::new(),
        }
    }

    #[test]
    fn heap_reports_small_double_free() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));
        assert_eq!(heap.dealloc(ptr, layout), Err(HeapError::DoubleFree));
    }

    #[test]
    fn heap_reports_small_realloc_after_free() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));
        assert_eq!(heap.realloc(ptr, layout, 128), Err(HeapError::DoubleFree));
    }

    #[test]
    fn heap_reuses_freed_large_extent_mapping() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let first = heap.alloc(layout);

        assert!(!first.is_null());
        assert_eq!(heap.dealloc(first, layout), Ok(()));

        let second = heap.alloc(layout);
        assert_eq!(second, first);
        assert_eq!(heap.dealloc(second, layout), Ok(()));
    }

    #[test]
    fn heap_reports_large_double_free_as_unknown_pointer_after_caching() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));
        assert_eq!(heap.dealloc(ptr, layout), Err(HeapError::UnknownPointer));
    }

    #[test]
    fn heap_reports_large_realloc_after_free_as_unknown_pointer_after_caching() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));
        assert_eq!(
            heap.realloc(ptr, layout, 512 * 1024),
            Err(HeapError::UnknownPointer)
        );
    }

    #[test]
    fn heap_zeroes_reused_run_block() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated for layout.size() bytes above.
        unsafe { write_bytes(ptr, 0xab, layout.size()) };
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));

        let zeroed = heap.alloc_zeroed(layout);
        assert!(!zeroed.is_null());
        // SAFETY: zeroed was allocated for layout.size() bytes above.
        let bytes = unsafe { core::slice::from_raw_parts(zeroed, layout.size()) };
        assert!(bytes.iter().all(|&byte| byte == 0));

        assert_eq!(heap.dealloc(zeroed, layout), Ok(()));
    }

    #[test]
    fn heap_zeroes_reused_extent_mapping() {
        let mut heap = test_heap();
        let layout = Layout::from_size_align(256 * 1024, 4096).unwrap();
        let ptr = heap.alloc(layout);

        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated for layout.size() bytes above.
        unsafe { write_bytes(ptr, 0xab, layout.size()) };
        assert_eq!(heap.dealloc(ptr, layout), Ok(()));

        let zeroed = heap.alloc_zeroed(layout);
        assert_eq!(zeroed, ptr);
        // SAFETY: zeroed was allocated for layout.size() bytes above.
        let bytes = unsafe { core::slice::from_raw_parts(zeroed, layout.size()) };
        assert!(bytes.iter().all(|&byte| byte == 0));

        assert_eq!(heap.dealloc(zeroed, layout), Ok(()));
    }
}
