use core::ptr::NonNull;

use crate::{
    arena::Arena,
    heap::extent::config::ExtentConfig,
    heap::{Extent, Heap, HeapError},
    layout::LayoutSpec,
    memory::{Mapping, OsMemory, PageMap, PageOwner},
};

use super::{ExtentId, ExtentInit, cache::ExtentCache};

pub(crate) struct ExtentHeap {
    extents: Arena<Extent>,
    cache: ExtentCache,
    /// Unmapped immortal slots, linked through [`Extent::next`].
    unmapped: Option<ExtentId>,
}

impl ExtentHeap {
    pub(crate) const fn new(config: ExtentConfig) -> Self {
        Self {
            extents: Arena::new(),
            cache: ExtentCache::new(config),
            unmapped: None,
        }
    }

    /// Borrow an extent slot for the process lifetime.
    ///
    /// Slots are immortal: [`Self::unmap`] drops only the mapping, the arena never
    /// removes a slot, and the heap arena backing it is mapped for the process
    /// lifetime. Page-map entries therefore stay valid for as long as they are stamped.
    fn slot(&self, id: ExtentId) -> Option<&'static Extent> {
        let extent = self.extents.get(id.index())?;
        // SAFETY: extent slots are never removed and their arena is never unmapped.
        Some(unsafe { &*core::ptr::from_ref(extent) })
    }

    /// Any occupied extent that is still Allocated or Claimed.
    ///
    /// Production reclaim uses [`Heap::occupied`] then this scan.
    /// Cached Free extents stay in the arena while published but are not live.
    pub(crate) fn has_live(&self) -> bool {
        self.extents.iter().any(Extent::is_live)
    }

    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        heap: &'static Heap,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(len) = spec.mapping_len(OsMemory::page_size()) else {
            return Ok(None);
        };
        if let Some(id) = self.cache.acquire(&self.extents, len)? {
            let Some(extent) = self.slot(id) else {
                return Err(HeapError::MissingExtent);
            };
            if let Some(ptr) = extent.reuse(spec, init) {
                heap.add_extent_live();
                return Ok(Some(ptr));
            }
            self.unmap(extent, pages)?;
        }

        let Some(mapping) = OsMemory::map(len) else {
            return Ok(None);
        };
        let Some(ptr) = self.allocate_mapping(spec, heap, mapping, pages) else {
            return Ok(None);
        };
        heap.add_extent_live();
        Ok(Some(ptr))
    }

    fn allocate_mapping(
        &mut self,
        spec: LayoutSpec,
        heap: &'static Heap,
        mapping: Mapping,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if let Some(extent) = self.pop_unmapped() {
            let Some(ptr) = extent.remount(mapping, spec) else {
                self.push_unmapped(extent);
                return None;
            };
            if pages.publish(PageOwner::Extent(extent)).is_err() {
                drop(extent.unmount());
                self.push_unmapped(extent);
                return None;
            }
            return Some(ptr);
        }

        let index = self.extents.vacant()?;
        let id = ExtentId::from_index(index)?;
        let extent = Extent::new(id, heap, mapping, spec)?;
        let ptr = extent.ptr();
        self.extents.insert(index, extent)?;
        self.insert_extent(id, pages)?;
        Some(ptr)
    }

    pub(crate) fn free(
        &mut self,
        extent: &'static Extent,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        extent.free(ptr)?;
        self.cache_or_unmap(extent, pages)
    }

    pub(crate) fn accept(
        &mut self,
        extent: &'static Extent,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        extent.accept(ptr)?;
        self.cache_or_unmap(extent, pages)
    }

    /// After free/accept: Keep/Discard retain published in cache; Unmap / over-budget unpublish.
    fn cache_or_unmap(
        &mut self,
        extent: &'static Extent,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        debug_assert!(!extent.is_live());
        extent.heap().sub_extent_live();
        if self.cache.insert(extent) {
            return Ok(());
        }

        self.unmap(extent, pages)
    }

    fn unmap(&mut self, extent: &'static Extent, pages: &PageMap) -> Result<(), HeapError> {
        pages
            .unpublish(PageOwner::Extent(extent))
            .map_err(|_| HeapError::InvalidMetadata)?;
        drop(extent.unmount());
        self.push_unmapped(extent);
        Ok(())
    }

    fn push_unmapped(&mut self, extent: &Extent) {
        extent.set_next(self.unmapped);
        self.unmapped = Some(extent.id());
    }

    fn pop_unmapped(&mut self) -> Option<&'static Extent> {
        let extent = self.slot(self.unmapped?)?;
        self.unmapped = extent.next();
        extent.set_next(None);
        Some(extent)
    }

    fn insert_extent(&mut self, id: ExtentId, pages: &PageMap) -> Option<()> {
        let inserted = self.slot(id)?;
        debug_assert_eq!(inserted.id(), id);
        if pages.publish(PageOwner::Extent(inserted)).is_err() {
            drop(inserted.unmount());
            self.push_unmapped(inserted);
            return None;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, ptr::write_bytes};

    use crate::{
        config::{AllocatorConfig, Budget},
        heap::extent::config::{ExtentConfig, ExtentPolicy},
        heap::{Extent, Heap, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
    };

    use super::super::LAZY_ZERO;
    use super::*;

    static OWNER: Heap = Heap::new(
        HeapId::new(0, core::num::NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = layout_spec(65_536, 8);
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, &OWNER, mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_keeps_immortal_slot_unmapped() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let index = heap.extents.vacant().unwrap();
        let id = ExtentId::from_index(index).unwrap();
        assert!(heap.extents.insert(index, reusable_extent(id)).is_some());
        let slot = heap.slot(id).unwrap();
        let base = slot.mapping().range().base();
        // Occupy the pages first; publication must fail closed.
        pages.publish(PageOwner::Extent(slot)).unwrap();

        assert!(heap.insert_extent(id, &pages).is_none());

        // Slots are immortal: only the mapping is dropped.
        assert!(heap.extents.get(index).is_some());
        assert!(heap.extents.get(index).unwrap().take_mapping().is_none());
        assert_eq!(pages.get(base), Some(PageOwner::Extent(slot)));
    }

    #[test]
    fn keep_free_leaves_page_map_entry_published() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let ptr = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        heap.free(extent, ptr, &pages).unwrap();

        assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
        assert!(!heap.has_live());
    }

    #[test]
    fn keep_cache_hit_reuses_the_exact_length_from_the_middle() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let small = layout_spec(64 * 1024, 4096);
        let medium = layout_spec(128 * 1024, 4096);
        let large = layout_spec(256 * 1024, 4096);

        let first = heap
            .allocate(small, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        let second = heap
            .allocate(medium, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        let third = heap
            .allocate(large, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        for ptr in [first, second, third] {
            let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
                panic!("expected extent owner");
            };
            heap.free(extent, ptr, &pages).unwrap();
        }

        // Cache holds large, medium, small; each request must unlink its own length.
        assert_eq!(
            heap.allocate(medium, &OWNER, &pages, ExtentInit::Uninit),
            Ok(Some(second))
        );
        assert_eq!(
            heap.allocate(small, &OWNER, &pages, ExtentInit::Uninit),
            Ok(Some(first))
        );
        assert_eq!(
            heap.allocate(large, &OWNER, &pages, ExtentInit::Uninit),
            Ok(Some(third))
        );
    }

    #[test]
    fn keep_slot_budget_unmaps_the_free_that_does_not_fit() {
        let mut heap =
            ExtentHeap::new(ExtentConfig::new().with_budget(Budget::new(2, 64 * 1024 * 1024)));
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let ptrs = [
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
        ];
        for ptr in ptrs {
            let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
                panic!("expected extent owner");
            };
            heap.free(extent, ptr, &pages).unwrap();
        }

        assert!(pages.get(ptrs[0]).is_some());
        assert!(pages.get(ptrs[1]).is_some());
        assert!(pages.get(ptrs[2]).is_none());
    }

    #[test]
    fn keep_byte_budget_unmaps_the_free_that_does_not_fit() {
        let spec = layout_spec(128 * 1024, 4096);
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mut heap = ExtentHeap::new(ExtentConfig::new().with_budget(Budget::new(64, 2 * len)));
        let pages = PageMap::new();
        let ptrs = [
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
            heap.allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
                .unwrap()
                .unwrap(),
        ];
        for ptr in ptrs {
            let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
                panic!("expected extent owner");
            };
            heap.free(extent, ptr, &pages).unwrap();
        }

        assert!(pages.get(ptrs[0]).is_some());
        assert!(pages.get(ptrs[1]).is_some());
        assert!(pages.get(ptrs[2]).is_none());
    }

    #[test]
    fn keep_cache_hit_reuses_without_republish() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let first = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        assert_eq!(reused, first);
        assert_eq!(pages.get(reused), Some(PageOwner::Extent(extent)));
    }

    #[test]
    fn unmap_policy_unpublishes_on_free() {
        let mut heap = ExtentHeap::new(ExtentConfig::new().with_policy(ExtentPolicy::Unmap));
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let ptr = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
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
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let ptr = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
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
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let size = 128 * 1024;
        let first = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
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
    fn keep_lazy_zero_second_reuse_is_clean() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(LAZY_ZERO, 4096);
        let first = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        // SAFETY: first is valid for LAZY_ZERO bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, LAZY_ZERO) };
        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let second = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        assert_eq!(second, first);
        // SAFETY: second is a Zeroed reuse of the same mapping.
        assert!(
            unsafe { core::slice::from_raw_parts(second.as_ptr(), LAZY_ZERO) }
                .iter()
                .all(|&byte| byte == 0)
        );
        // SAFETY: dirty the mapping again so a stale skip_zero would leak.
        unsafe { write_bytes(second.as_ptr(), 0xcd, LAZY_ZERO) };
        heap.free(extent, second, &pages).unwrap();

        let third = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        assert_eq!(third, first);
        // SAFETY: third must be zero even after a prior Keep lazy-zero discard.
        assert!(
            unsafe { core::slice::from_raw_parts(third.as_ptr(), LAZY_ZERO) }
                .iter()
                .all(|&byte| byte == 0)
        );
        heap.free(extent, third, &pages).unwrap();
    }

    #[test]
    fn discard_zeroed_allocate_is_clean_after_dirty_reuse() {
        let mut heap = ExtentHeap::new(ExtentConfig::new().with_policy(ExtentPolicy::Discard));
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let size = 128 * 1024;
        let first = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();
        assert_eq!(pages.get(first), Some(PageOwner::Extent(extent)));

        let reused = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Zeroed)
            .unwrap()
            .unwrap();
        assert_eq!(reused, first);
        // SAFETY: reused is valid for size bytes; Discard must yield zeros without Keep memset.
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
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let size = 128 * 1024;
        let first = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xcd, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, &OWNER, &pages, ExtentInit::Uninit)
            .unwrap()
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
