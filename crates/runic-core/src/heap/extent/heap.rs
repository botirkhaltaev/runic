use core::ptr::{NonNull, write_bytes};

use crate::{
    arena::Arena,
    config::ExtentConfig,
    heap::{Extent, HeapId},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
};

use super::{ExtentError, ExtentId, cache::ExtentCache};

pub(crate) struct ExtentHeap {
    extents: Arena<Extent>,
    cache: ExtentCache,
}

/// How a newly allocated extent's bytes should be initialized.
///
/// Fresh anonymous mappings are already kernel-zeroed. Cached mappings may be
/// dirty, so [`ExtentInit::Zeroed`] only memsets on cache hits (using
/// [`LayoutSpec::size`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentInit {
    Uninit,
    Zeroed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentHeapError {
    MissingExtent,
    InvalidPointer,
    InvalidMetadata,
    DoubleFree,
}

impl From<ExtentError> for ExtentHeapError {
    fn from(error: ExtentError) -> Self {
        match error {
            ExtentError::InvalidPointer => Self::InvalidPointer,
            ExtentError::DoubleFree => Self::DoubleFree,
        }
    }
}

impl ExtentHeap {
    pub(crate) fn new(capacity: u32, config: ExtentConfig) -> Self {
        Self {
            extents: Arena::new(capacity),
            cache: ExtentCache::new(config),
        }
    }

    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        heap_id: HeapId,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        let (mapping, cached) = if let Some(mapping) = self.cache.take(len) {
            (mapping, true)
        } else {
            (OsMemory::map(len)?, false)
        };

        let ptr = self.allocate_mapping(spec, heap_id, mapping, pages)?;
        if cached && init == ExtentInit::Zeroed {
            // SAFETY: ptr was just allocated for spec and is valid for spec.size() bytes.
            unsafe { write_bytes(ptr.as_ptr(), 0, spec.size()) };
        }

        Some(ptr)
    }

    fn allocate_mapping(
        &mut self,
        spec: LayoutSpec,
        heap_id: HeapId,
        mapping: crate::memory::Mapping,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let index = self.extents.claim()?;
        let Some(id) = ExtentId::from_index(u32::try_from(index).ok()?) else {
            self.extents.release(index);
            return None;
        };
        let Some(extent) = Extent::new(id, heap_id, mapping, spec) else {
            self.extents.release(index);
            return None;
        };
        debug_assert_eq!(extent.id(), id);
        let ptr = extent.ptr();

        self.insert_extent(index, id, extent, pages)?;

        Some(ptr)
    }

    pub(crate) fn complete_remote_free(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        Self::validate_remote_free(extent_ptr, ptr)?;
        self.retire(extent_ptr, pages)
    }

    pub(crate) fn free(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        unsafe { extent_ptr.as_ref() }
            .free(ptr)
            .map_err(ExtentHeapError::from)?;
        self.retire(extent_ptr, pages)
    }

    /// Validate a remote-pending free before the shared retire path.
    ///
    /// The remote-free protocol already transitioned the extent to
    /// `RemotePending` via `claim_free`; this only confirms that state and the
    /// exact pointer before the entry is torn down.
    fn validate_remote_free(
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
    ) -> Result<(), ExtentHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let extent = unsafe { extent_ptr.as_ref() };
        extent
            .validate_remote_pending()
            .map_err(ExtentHeapError::from)?;
        extent.validate_free(ptr).map_err(ExtentHeapError::from)?;
        Ok(())
    }

    /// One retire path shared by local and remote-completed frees: unpublish the
    /// page-map entry, remove the arena slot, and offer the mapping to the cache.
    fn retire(
        &mut self,
        extent_ptr: NonNull<Extent>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let extent = unsafe { extent_ptr.as_ref() };
        let id = extent.id();
        let range = extent.mapping_range();

        pages
            .unpublish_extent(range, extent_ptr)
            .map_err(|_| ExtentHeapError::InvalidMetadata)?;

        let index = usize::try_from(id.index()).map_err(|_| ExtentHeapError::InvalidMetadata)?;
        let Some(extent) = self.extents.remove(index) else {
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
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let extent = unsafe { extent.as_mut() };

        extent
            .resize_in_place(ptr, spec)
            .map_err(ExtentHeapError::from)
    }

    fn insert_extent(
        &mut self,
        index: usize,
        id: ExtentId,
        extent: Extent,
        pages: &PageMap,
    ) -> Option<NonNull<Extent>> {
        let range = extent.mapping_range();

        if self.extents.insert(index, extent).is_none() {
            self.extents.release(index);
            return None;
        }

        let Some(inserted_extent) = self.extents.get_mut(index) else {
            let _removed = self.extents.remove(index);
            return None;
        };
        debug_assert_eq!(inserted_extent.id(), id);
        let extent_ptr = NonNull::from(&mut *inserted_extent);

        if pages.publish_extent(range, extent_ptr).is_err() {
            let _removed = self.extents.remove(index);
            return None;
        }

        Some(extent_ptr)
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::write_bytes;

    use core::num::NonZeroU32;

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

        Extent::new(id, HeapId::new(0, NonZeroU32::MIN).unwrap(), mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_table_entry() {
        let mut allocator = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let index = allocator.extents.claim().unwrap();
        let id = ExtentId::from_index(u32::try_from(index).unwrap()).unwrap();
        let extent = reusable_extent(id);
        let range = extent.mapping_range();
        let existing = NonNull::dangling();

        pages.publish_extent(range, existing).unwrap();

        assert_eq!(allocator.insert_extent(index, id, extent, &pages), None);
        assert!(allocator.extents.get_mut(index).is_none());
        assert_eq!(pages.get(range.base()), Some(PageOwner::Extent(existing)));
    }

    #[test]
    fn zeroed_allocate_clears_cached_mapping() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let size = 128 * 1024;
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let first = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Zeroed)
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Zeroed)
            .unwrap();
        assert_eq!(reused, first);
        // SAFETY: reused is valid for size bytes.
        assert!(
            unsafe { core::slice::from_raw_parts(reused.as_ptr(), size) }
                .iter()
                .all(|&byte| byte == 0)
        );

        let Some(PageOwner::Extent(extent)) = pages.get(reused) else {
            panic!("expected extent owner");
        };
        heap.free(extent, reused, &pages).unwrap();
    }

    #[test]
    fn uninit_allocate_preserves_cached_bytes() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let size = 128 * 1024;
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let first = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xcd, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        assert_eq!(reused, first);
        // SAFETY: reused is valid for size bytes.
        assert!(
            unsafe { core::slice::from_raw_parts(reused.as_ptr(), size) }
                .iter()
                .all(|&byte| byte == 0xcd)
        );

        let Some(PageOwner::Extent(extent)) = pages.get(reused) else {
            panic!("expected extent owner");
        };
        heap.free(extent, reused, &pages).unwrap();
    }
}
