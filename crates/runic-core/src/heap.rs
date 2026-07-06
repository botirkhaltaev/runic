use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
};

use crate::{
    extent::Extent,
    extent_table::{ExtentReservation, ExtentTable},
    layout::LayoutSpec,
    mapping_cache::MappingCache,
    os_memory::OsMemory,
    page_map::{L2TablePolicy, PageEntry, PageMap, PageRange},
    run::{RUN_SIZE, Run, RunError, RunId},
    run_table::{RunReservation, RunTable},
    size_class::{SizeClass, SizeClasses},
};

pub(crate) struct Heap {
    runs: RunTable,
    extents: ExtentTable,
    mapping_cache: MappingCache,
    pages: PageMap,
    active: [Option<RunId>; SizeClasses::COUNT],
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
            runs: RunTable::new(Self::DEFAULT_TABLE_CAPACITY),
            extents: ExtentTable::new(Self::DEFAULT_TABLE_CAPACITY),
            mapping_cache: MappingCache::new(),
            pages: PageMap::new(),
            active: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);

        self.alloc_spec(spec)
    }

    pub(crate) fn dealloc(&mut self, raw_ptr: *mut u8, _layout: Layout) -> Result<(), HeapError> {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return Ok(());
        };

        let Some(entry) = self.pages.get(ptr) else {
            return Err(HeapError::UnknownPointer);
        };

        match entry {
            PageEntry::Run(id) => {
                let class_index = {
                    let Some(run) = self.runs.get_mut(id) else {
                        return Err(HeapError::MissingRun);
                    };

                    run.free(ptr).map_err(HeapError::from)?;

                    run.class().index()
                };

                let Some(active_slot) = self.active.get_mut(class_index) else {
                    return Err(HeapError::InvalidMetadata);
                };
                *active_slot = Some(id);
            }
            PageEntry::Extent(id) => {
                let (range, mapping_len) = {
                    let Some(extent) = self.extents.get(id) else {
                        return Err(HeapError::MissingExtent);
                    };

                    if extent.free(ptr).is_err() {
                        return Err(HeapError::InvalidExtentPointer);
                    }

                    (extent.range(), extent.mapping_len())
                };

                let retain_mapping = self.mapping_cache.can_retain(mapping_len);
                let Some(page_range) = PageRange::new(range.base(), range.len()) else {
                    return Err(HeapError::InvalidMetadata);
                };

                let empty_l2_tables = if retain_mapping {
                    L2TablePolicy::RetainEmpty
                } else {
                    L2TablePolicy::ReleaseEmpty
                };
                self.pages
                    .remove(page_range, PageEntry::Extent(id), empty_l2_tables)
                    .map_err(|_| HeapError::InvalidMetadata)?;

                let Some(extent) = self.extents.remove(id) else {
                    return Err(HeapError::MissingExtent);
                };

                let mapping = extent.into_mapping();
                if let Err(mapping) = self.mapping_cache.insert(mapping) {
                    drop(mapping);
                }
            }
        }

        Ok(())
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

            return Ok(self.alloc_spec(spec));
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
            PageEntry::Run(id) => {
                let Some(run) = self.runs.get(id) else {
                    return Err(HeapError::MissingRun);
                };

                run.allocated_block_at(old_ptr).map_err(HeapError::from)?;
            }
            PageEntry::Extent(id) => {
                let Some(extent) = self.extents.get(id) else {
                    return Err(HeapError::MissingExtent);
                };

                if !extent.starts_at(old_ptr) {
                    return Err(HeapError::InvalidExtentPointer);
                }
            }
        }

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return Ok(null_mut());
        };
        let new_spec = LayoutSpec::from_layout(new_layout);
        let new_ptr = self.alloc_spec(new_spec);

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
        let ptr = self.alloc(layout);

        if !ptr.is_null() {
            // SAFETY: ptr is valid for layout.size() bytes because it was just allocated for layout.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }

        ptr
    }

    fn alloc_spec(&mut self, spec: LayoutSpec) -> *mut u8 {
        match SizeClasses::get(spec) {
            Some(class) => self.alloc_small(spec, class),
            None => self.alloc_large(spec),
        }
    }

    fn alloc_small(&mut self, spec: LayoutSpec, class: SizeClass) -> *mut u8 {
        let class_index = class.id().index();
        let active_id = self.active.get(class_index).copied().flatten();

        if let Some(id) = active_id
            && let Some(run) = self.runs.get_mut(id)
            && let Some(ptr) = run.allocate(spec)
        {
            return ptr.as_ptr();
        }

        if let Some((id, ptr)) = self.runs.allocate(class, spec) {
            let Some(active_slot) = self.active.get_mut(class_index) else {
                return null_mut();
            };
            *active_slot = Some(id);

            return ptr.as_ptr();
        }

        let Some(mapping) = OsMemory::map(RUN_SIZE) else {
            return null_mut();
        };
        let Some(reservation) = self.runs.reserve() else {
            return null_mut();
        };
        let id = reservation.id();

        let run = Run::new(id, mapping, class);
        if self.insert_run(reservation, run).is_err() {
            return null_mut();
        }

        let Some(active_slot) = self.active.get_mut(class_index) else {
            return null_mut();
        };
        *active_slot = Some(id);

        let Some(inserted_run) = self.runs.get_mut(id) else {
            return null_mut();
        };

        inserted_run
            .allocate(spec)
            .map_or(null_mut(), NonNull::as_ptr)
    }

    fn alloc_large(&mut self, spec: LayoutSpec) -> *mut u8 {
        let Some(len) = spec.mapping_len(OsMemory::page_size()) else {
            return null_mut();
        };
        let mapping = if let Some(mapping) = self.mapping_cache.take_exact(len) {
            mapping
        } else {
            let Some(mapping) = OsMemory::map(len) else {
                return null_mut();
            };

            mapping
        };

        let Some(reservation) = self.extents.reserve() else {
            return null_mut();
        };
        let id = reservation.id();
        let Some(extent) = Extent::new(id, mapping, spec) else {
            self.extents.release(reservation);
            return null_mut();
        };
        debug_assert_eq!(extent.id(), id, "new extent should keep its reserved id");
        let ptr = extent.ptr();

        if self.insert_extent(reservation, extent).is_err() {
            return null_mut();
        }

        ptr.as_ptr()
    }

    fn insert_run(&mut self, reservation: RunReservation, run: Run) -> Result<RunId, ()> {
        let id = reservation.id();
        let range = run.range();

        if self.runs.insert(reservation, run).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            let _removed = self.runs.remove(id);
            return Err(());
        };

        if self.pages.insert(page_range, PageEntry::Run(id)).is_err() {
            let _removed = self.runs.remove(id);
            return Err(());
        }

        Ok(id)
    }

    fn insert_extent(&mut self, reservation: ExtentReservation, extent: Extent) -> Result<(), ()> {
        let id = reservation.id();
        let range = extent.range();

        if self.extents.insert(reservation, extent).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            let _removed = self.extents.remove(id);
            return Err(());
        };

        if self
            .pages
            .insert(page_range, PageEntry::Extent(id))
            .is_err()
        {
            let _removed = self.extents.remove(id);
            return Err(());
        }

        Ok(())
    }
}

impl From<RunError> for HeapError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidPointer => Self::InvalidRunPointer,
            RunError::DoubleFree => Self::DoubleFree,
            RunError::FreeUnderflow => Self::InvalidMetadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{extent::ExtentId, page_map::PageRange, run::RunId, size_class::SizeClasses};

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::get(spec).unwrap();

        Run::new(id, mapping, class)
    }

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, mapping, spec).unwrap()
    }

    fn test_heap() -> Heap {
        Heap {
            runs: RunTable::new(4),
            extents: ExtentTable::new(4),
            mapping_cache: MappingCache::new(),
            pages: PageMap::new(),
            active: [None; SizeClasses::COUNT],
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
    fn failed_run_page_publication_removes_table_entry() {
        let mut heap = test_heap();
        let reservation = heap.runs.reserve().unwrap();
        let id = reservation.id();
        let run = reusable_run(id);
        let range = run.range();
        let page_range = PageRange::new(range.base(), range.len()).unwrap();
        let existing = PageEntry::Run(RunId::from_index(900).unwrap());

        heap.pages.insert(page_range, existing).unwrap();

        assert_eq!(heap.insert_run(reservation, run), Err(()));
        assert!(heap.runs.get(id).is_none());
        assert_eq!(heap.pages.get(range.base()), Some(existing));
    }

    #[test]
    fn failed_extent_page_publication_removes_table_entry() {
        let mut heap = test_heap();
        let reservation = heap.extents.reserve().unwrap();
        let id = reservation.id();
        let extent = reusable_extent(id);
        let range = extent.range();
        let page_range = PageRange::new(range.base(), range.len()).unwrap();
        let existing = PageEntry::Extent(ExtentId::from_index(900).unwrap());

        heap.pages.insert(page_range, existing).unwrap();

        assert_eq!(heap.insert_extent(reservation, extent), Err(()));
        assert!(heap.extents.get(id).is_none());
        assert_eq!(heap.pages.get(range.base()), Some(existing));
    }
}
