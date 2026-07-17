use core::ptr::NonNull;

use crate::{
    allocation::{Allocation, ZeroStatus},
    config::ExtentConfig,
    extent::{Extent, ExtentArena},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
    ownership::HeapOwner,
};

use super::{ExtentReservation, cache::ExtentCache};

pub(crate) struct ExtentHeap {
    extents: ExtentArena,
    cache: ExtentCache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentHeapError {
    MissingExtent,
    InvalidPointer,
    InvalidMetadata,
}

impl ExtentHeap {
    pub(crate) const fn new(capacity: u32, config: ExtentConfig) -> Self {
        Self {
            extents: ExtentArena::new(capacity),
            cache: ExtentCache::new(config),
        }
    }

    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        owner: HeapOwner,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        let (mapping, zero_status) = if let Some(mapping) = self.cache.take(len) {
            (mapping, ZeroStatus::NeedsZeroing)
        } else {
            (OsMemory::map(len)?, ZeroStatus::KnownZeroed)
        };

        let reservation = self.extents.reserve()?;
        let id = reservation.id();
        let Some(extent) = Extent::new(id, owner, mapping, spec) else {
            self.extents.release(reservation);
            return None;
        };
        debug_assert_eq!(extent.id(), id, "new extent should keep its reserved id");
        let ptr = extent.ptr();

        self.insert_extent(reservation, extent, pages)?;

        Some(Allocation::new(ptr, zero_status))
    }

    pub(crate) fn free(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), ExtentHeapError> {
        let (id, range, mapping_len) = {
            // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
            let extent = unsafe { extent_ptr.as_ref() };

            if extent.free(ptr).is_err() {
                return Err(ExtentHeapError::InvalidPointer);
            }

            (extent.id(), extent.mapping_range(), extent.mapping_len())
        };

        let _retain_mapping = self.cache.will_retain(mapping_len);
        pages
            .unpublish_extent(range, extent_ptr)
            .map_err(|_| ExtentHeapError::InvalidMetadata)?;

        let Some(extent) = self.extents.remove(id) else {
            return Err(ExtentHeapError::MissingExtent);
        };

        let mapping = extent.into_mapping();
        if let Err(mapping) = self.cache.insert(mapping) {
            drop(mapping);
        }

        Ok(())
    }

    pub(crate) fn resize_in_place(
        mut extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, ExtentHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
        let extent = unsafe { extent.as_mut() };

        extent
            .resize_in_place(ptr, spec)
            .map_err(|_| ExtentHeapError::InvalidPointer)
    }

    fn insert_extent(
        &mut self,
        reservation: ExtentReservation,
        extent: Extent,
        pages: &mut PageMap,
    ) -> Option<NonNull<Extent>> {
        let id = reservation.id();
        let range = extent.mapping_range();

        if self.extents.insert(reservation, extent).is_err() {
            return None;
        }

        let Some(inserted_extent) = self.extents.get_mut(id) else {
            let _removed = self.extents.remove(id);
            return None;
        };
        let extent_ptr = NonNull::from(&mut *inserted_extent);

        if pages.publish_extent(range, extent_ptr).is_err() {
            let _removed = self.extents.remove(id);
            return None;
        }

        Some(extent_ptr)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        extent::{Extent, ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, HeapOwner::Shared, mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_table_entry() {
        let mut allocator = ExtentHeap::new(4, ExtentConfig::new());
        let mut pages = PageMap::new();
        let reservation = allocator.extents.reserve().unwrap();
        let id = reservation.id();
        let extent = reusable_extent(id);
        let range = extent.mapping_range();
        let existing_extent = NonNull::dangling();
        let existing = PageOwner::Extent(existing_extent);

        pages.publish_extent(range, existing_extent).unwrap();

        assert_eq!(
            allocator.insert_extent(reservation, extent, &mut pages),
            None
        );
        assert!(allocator.extents.get_mut(id).is_none());
        assert_eq!(pages.get(range.base()), Some(existing));
    }
}
