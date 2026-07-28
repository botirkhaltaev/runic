use core::ptr::{NonNull, write_bytes};

use crate::{
    arena::Arena,
    config::ExtentConfig,
    heap::{Extent, HeapError, HeapId},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
};

use super::{ExtentId, cache::ExtentCache};

pub(crate) struct ExtentHeap {
    extents: Arena<Extent>,
    cache: ExtentCache,
}

/// How a newly allocated extent's bytes should be initialized.
///
/// Fresh anonymous mappings are already kernel-zeroed. Cached extents may be
/// dirty, so [`ExtentInit::Zeroed`] only memsets on cache hits (using
/// [`LayoutSpec::size`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentInit {
    Uninit,
    Zeroed,
}

impl ExtentHeap {
    pub(crate) fn new(capacity: u32, config: ExtentConfig) -> Self {
        Self {
            extents: Arena::new(capacity),
            cache: ExtentCache::new(config),
        }
    }

    /// Any occupied extent that is still Allocated or Claimed.
    ///
    /// Cached Free extents stay in the arena while published but are not live.
    pub(crate) fn has_live(&self) -> bool {
        let len = self.extents.len();
        for index in 0..len {
            if self.extents.get(index).is_some_and(Extent::is_live) {
                return true;
            }
        }
        false
    }

    pub(crate) fn rebind(&mut self, heap_id: HeapId) {
        let len = self.extents.len();
        for index in 0..len {
            let Some(extent) = self.extents.get_mut(index) else {
                continue;
            };
            extent.set_heap_id(heap_id);
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
        if let Some(mut extent_ptr) = self.cache.take(len) {
            // SAFETY: cache only stores live arena extents owned by this heap.
            let extent = unsafe { extent_ptr.as_mut() };
            if let Some(ptr) = extent.reuse(heap_id, spec) {
                if init == ExtentInit::Zeroed {
                    // SAFETY: ptr was just reused for spec and is valid for spec.size() bytes.
                    unsafe { write_bytes(ptr.as_ptr(), 0, spec.size()) };
                }
                return Some(ptr);
            }
            // Cache keyed by mapping length; reuse failure is rare (align) — release and remap.
            let _ = self.unmap(extent_ptr, pages);
        }

        let mapping = OsMemory::map(len)?;
        self.allocate_mapping(spec, heap_id, mapping, pages)
    }

    fn allocate_mapping(
        &mut self,
        spec: LayoutSpec,
        heap_id: HeapId,
        mapping: crate::memory::Mapping,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let index = self.extents.claim()?;
        let Some(id) = ExtentId::from_index(index) else {
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

    pub(crate) fn free(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        unsafe { extent_ptr.as_ref() }.free(ptr)?;
        self.cache_or_unmap(extent_ptr, pages)
    }

    pub(crate) fn accept(
        &mut self,
        extent_ptr: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        unsafe { extent_ptr.as_ref() }.accept(ptr)?;
        self.cache_or_unmap(extent_ptr, pages)
    }

    /// After free/accept: Keep retains published in cache; otherwise unpublish and drop.
    fn cache_or_unmap(
        &mut self,
        extent_ptr: NonNull<Extent>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let extent = unsafe { extent_ptr.as_ref() };
        debug_assert!(!extent.is_live());
        let len = extent.mapping().len().get();

        if self.cache.will_retain(len) && self.cache.insert(extent_ptr).is_ok() {
            return Ok(());
        }

        self.unmap(extent_ptr, pages)
    }

    fn unmap(&mut self, extent_ptr: NonNull<Extent>, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let extent = unsafe { extent_ptr.as_ref() };
        let id = extent.id();

        pages
            .unpublish_extent(extent.mapping(), extent_ptr)
            .map_err(|_| HeapError::InvalidMetadata)?;

        let index = id.index();
        let Some(extent) = self.extents.remove(index) else {
            return Err(HeapError::MissingExtent);
        };

        drop(extent.into_mapping());
        Ok(())
    }

    fn insert_extent(
        &mut self,
        index: u32,
        id: ExtentId,
        extent: Extent,
        pages: &PageMap,
    ) -> Option<NonNull<Extent>> {
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

        if pages
            .publish_extent(inserted_extent.mapping(), extent_ptr)
            .is_err()
        {
            let _removed = self.extents.remove(index);
            return None;
        }

        Some(extent_ptr)
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, num::NonZeroU32, ptr::write_bytes};

    use crate::{
        config::{ExtentConfig, ExtentPolicy},
        heap::{Extent, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
    };

    use super::*;

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = layout_spec(65_536, 8);
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, HeapId::new(0, NonZeroU32::MIN).unwrap(), mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_map_entry() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let index = heap.extents.claim().unwrap();
        let id = ExtentId::from_index(index).unwrap();
        let extent = reusable_extent(id);
        let existing = NonNull::dangling();
        let base = extent.mapping().range().base();

        pages.publish_extent(extent.mapping(), existing).unwrap();

        assert_eq!(heap.insert_extent(index, id, extent, &pages), None);
        assert!(heap.extents.get_mut(index).is_none());
        assert_eq!(pages.get(base), Some(PageOwner::Extent(existing)));
    }

    #[test]
    fn keep_free_leaves_page_map_entry_published() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let ptr = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        heap.free(extent, ptr, &pages).unwrap();

        assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
        assert!(!heap.has_live());
    }

    #[test]
    fn keep_cache_hit_reuses_without_republish() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let first = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        assert_eq!(reused, first);
        assert_eq!(pages.get(reused), Some(PageOwner::Extent(extent)));
    }

    #[test]
    fn drop_policy_unpublishes_on_free() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new().with_policy(ExtentPolicy::Drop));
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let ptr = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        heap.free(extent, ptr, &pages).unwrap();

        assert!(pages.get(ptr).is_none());
        assert!(!heap.has_live());
    }

    #[test]
    fn double_free_while_cached_is_rejected() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();

        let ptr = heap
            .allocate(spec, heap_id, &pages, ExtentInit::Uninit)
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        heap.free(extent, ptr, &pages).unwrap();

        assert_eq!(heap.free(extent, ptr, &pages), Err(HeapError::DoubleFree));
        assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
    }

    #[test]
    fn zeroed_allocate_clears_cached_mapping() {
        let mut heap = ExtentHeap::new(4, ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
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
        let spec = layout_spec(128 * 1024, 4096);
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
