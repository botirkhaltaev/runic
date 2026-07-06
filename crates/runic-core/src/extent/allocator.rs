use core::ptr::NonNull;

use crate::{
    allocation::{Allocation, ZeroStatus},
    extent::{Extent, ExtentId, ExtentTable},
    layout::LayoutSpec,
    memory::{L2TablePolicy, OsMemory, PageEntry, PageMap, PageRange},
};

use super::{ExtentReservation, mapping_cache::MappingCache};

pub(crate) struct ExtentAllocator {
    extents: ExtentTable,
    mapping_cache: MappingCache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentAllocatorError {
    MissingExtent,
    InvalidPointer,
    InvalidMetadata,
}

impl ExtentAllocator {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            extents: ExtentTable::new(capacity),
            mapping_cache: MappingCache::new(),
        }
    }

    pub(crate) fn allocate(&mut self, spec: LayoutSpec, pages: &mut PageMap) -> Option<Allocation> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        let (mapping, zero_status) = if let Some(mapping) = self.mapping_cache.take_exact(len) {
            (mapping, ZeroStatus::NeedsZeroing)
        } else {
            (OsMemory::map(len)?, ZeroStatus::KnownZeroed)
        };

        let reservation = self.extents.reserve()?;
        let id = reservation.id();
        let Some(extent) = Extent::new(id, mapping, spec) else {
            self.extents.release(reservation);
            return None;
        };
        debug_assert_eq!(extent.id(), id, "new extent should keep its reserved id");
        let ptr = extent.ptr();

        if self.insert_extent(reservation, extent, pages).is_err() {
            return None;
        }

        Some(Allocation::new(ptr, zero_status))
    }

    pub(crate) fn free(
        &mut self,
        id: ExtentId,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), ExtentAllocatorError> {
        let (range, mapping_len) = {
            let Some(extent) = self.extents.get(id) else {
                return Err(ExtentAllocatorError::MissingExtent);
            };

            if extent.free(ptr).is_err() {
                return Err(ExtentAllocatorError::InvalidPointer);
            }

            (extent.range(), extent.mapping_len())
        };

        let retain_mapping = self.mapping_cache.can_retain(mapping_len);
        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            return Err(ExtentAllocatorError::InvalidMetadata);
        };

        let empty_l2_tables = if retain_mapping {
            L2TablePolicy::RetainEmpty
        } else {
            L2TablePolicy::ReleaseEmpty
        };
        pages
            .remove(page_range, PageEntry::Extent(id), empty_l2_tables)
            .map_err(|_| ExtentAllocatorError::InvalidMetadata)?;

        let Some(extent) = self.extents.remove(id) else {
            return Err(ExtentAllocatorError::MissingExtent);
        };

        let mapping = extent.into_mapping();
        if let Err(mapping) = self.mapping_cache.insert(mapping) {
            drop(mapping);
        }

        Ok(())
    }

    pub(crate) fn validate_allocated(
        &self,
        id: ExtentId,
        ptr: NonNull<u8>,
    ) -> Result<(), ExtentAllocatorError> {
        let Some(extent) = self.extents.get(id) else {
            return Err(ExtentAllocatorError::MissingExtent);
        };

        if !extent.starts_at(ptr) {
            return Err(ExtentAllocatorError::InvalidPointer);
        }

        Ok(())
    }

    fn insert_extent(
        &mut self,
        reservation: ExtentReservation,
        extent: Extent,
        pages: &mut PageMap,
    ) -> Result<(), ()> {
        let id = reservation.id();
        let range = extent.range();

        if self.extents.insert(reservation, extent).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            let _removed = self.extents.remove(id);
            return Err(());
        };

        if pages.insert(page_range, PageEntry::Extent(id)).is_err() {
            let _removed = self.extents.remove(id);
            return Err(());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        extent::{Extent, ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageEntry, PageMap, PageRange},
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_table_entry() {
        let mut allocator = ExtentAllocator::new(4);
        let mut pages = PageMap::new();
        let reservation = allocator.extents.reserve().unwrap();
        let id = reservation.id();
        let extent = reusable_extent(id);
        let range = extent.range();
        let page_range = PageRange::new(range.base(), range.len()).unwrap();
        let existing = PageEntry::Extent(ExtentId::from_index(900).unwrap());

        pages.insert(page_range, existing).unwrap();

        assert_eq!(
            allocator.insert_extent(reservation, extent, &mut pages),
            Err(())
        );
        assert!(allocator.extents.get(id).is_none());
        assert_eq!(pages.get(range.base()), Some(existing));
    }
}
