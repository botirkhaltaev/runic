use core::ptr::{NonNull, write_bytes};

use crate::{
    config::ExtentConfig,
    heap::{Extent, ExtentArena, Owner},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
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
        owner: Owner,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        let mapping = self.cache.take(len).or_else(|| OsMemory::map(len))?;

        self.allocate_mapping(spec, owner, mapping, pages)
    }

    pub(crate) fn allocate_zeroed(
        &mut self,
        spec: LayoutSpec,
        requested_size: usize,
        owner: Owner,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        let (mapping, needs_zeroing) = if let Some(mapping) = self.cache.take(len) {
            (mapping, true)
        } else {
            (OsMemory::map(len)?, false)
        };

        let ptr = self.allocate_mapping(spec, owner, mapping, pages)?;
        if needs_zeroing {
            // SAFETY: ptr was just allocated for spec and is valid for the requested layout size.
            unsafe { write_bytes(ptr.as_ptr(), 0, requested_size) };
        }

        Some(ptr)
    }

    fn allocate_mapping(
        &mut self,
        spec: LayoutSpec,
        owner: Owner,
        mapping: crate::memory::Mapping,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let reservation = self.extents.reserve()?;
        let id = reservation.id;
        let Some(extent) = Extent::new(id, owner, mapping, spec) else {
            self.extents.release(reservation);
            return None;
        };
        debug_assert_eq!(extent.id(), id, "new extent should keep its reserved id");
        let ptr = extent.ptr();

        self.insert_extent(reservation, extent, pages)?;

        Some(ptr)
    }

    pub(crate) fn free(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        let (id, range) = {
            // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
            let extent = unsafe { extent_ptr.as_ref() };

            if extent.free(ptr).is_err() {
                return Err(ExtentHeapError::InvalidPointer);
            }

            (extent.id(), extent.mapping_range())
        };

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
        pages: &PageMap,
    ) -> Option<NonNull<Extent>> {
        let id = reservation.id;
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
        heap::{Extent, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, Owner::for_heap(HeapId::ROOT), mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_table_entry() {
        let mut allocator = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let reservation = allocator.extents.reserve().unwrap();
        let id = reservation.id;
        let extent = reusable_extent(id);
        let range = extent.mapping_range();
        let existing = NonNull::dangling();

        pages.publish_extent(range, existing).unwrap();

        assert_eq!(allocator.insert_extent(reservation, extent, &pages), None);
        assert!(allocator.extents.get_mut(id).is_none());
        assert_eq!(pages.get(range.base()), Some(PageOwner::Extent(existing)));
    }
}
