use core::ptr::NonNull;

use crate::{
    arena::Arena,
    config::Hints,
    heap::extent::config::ExtentConfig,
    heap::{Extent, Heap, HeapError},
    layout::LayoutSpec,
    memory::{Mapping, Memory, Os, PageMap, PageOwner},
};

use super::super::list::LinkedList;
use super::{ExtentInit, cache::ExtentCache};

pub(crate) struct ExtentHeap {
    extents: Arena<Extent>,
    cache: ExtentCache,
    /// Unmapped immortal slots. Shares [`Extent`]'s list link with the cache.
    unmapped: LinkedList<'static, Extent>,
    hints: Hints,
}

impl ExtentHeap {
    pub(crate) const fn new(config: ExtentConfig, hints: Hints) -> Self {
        Self {
            extents: Arena::new(),
            cache: ExtentCache::new(config),
            unmapped: LinkedList::new(),
            hints,
        }
    }

    /// Any live extent: one still Allocated or Claimed.
    ///
    /// Production reclaim uses [`Heap::is_live`] then this scan.
    /// Cached Free extents stay in the arena while published but are not live.
    pub(crate) fn has_live(&self) -> bool {
        self.extents.iter().any(Extent::is_live)
    }

    /// Cached reuse, else a fresh mapping. Fresh pages are zero.
    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        heap: &'static Heap,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        if let Some(ptr) = self.reuse_cached(spec, heap, pages, init)? {
            return Ok(Some(ptr));
        }
        let Some(len) = spec.mapping_len(Os::page_size()) else {
            return Ok(None);
        };
        let Some(mapping) = Os::map_payload(len, self.hints) else {
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
        if let Some(extent) = self.unmapped.pop_front() {
            if extent.remount(mapping, spec).is_none() {
                self.unmapped.push_front(extent);
                return None;
            }
            return self.publish(extent, pages);
        }

        let index = self.extents.vacant()?;
        let extent = Extent::new(heap, mapping, spec)?;
        let inserted = self.extents.insert(index, extent)?;
        // SAFETY: extent slots are never removed and their arena is never unmapped,
        // so a published page-map stamp stays valid for the process lifetime.
        let inserted = unsafe { &*core::ptr::from_mut(inserted).cast_const() };
        self.publish(inserted, pages)
    }

    /// Stamp the page map. On failure drop the mapping and keep the immortal slot.
    fn publish(&mut self, extent: &'static Extent, pages: &PageMap) -> Option<NonNull<u8>> {
        if pages.publish(PageOwner::Extent(extent)).is_err() {
            drop(extent.unmount());
            self.unmapped.push_front(extent);
            return None;
        }
        Some(extent.ptr())
    }

    /// Exact-length cache hit. Does not map. A reuse failure unmaps that slot.
    pub(crate) fn reuse_cached(
        &mut self,
        spec: LayoutSpec,
        heap: &'static Heap,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        let Some(len) = spec.mapping_len(Os::page_size()) else {
            return Ok(None);
        };
        let Some(extent) = self.cache.take(len) else {
            return Ok(None);
        };
        if let Some(ptr) = extent.reuse(spec, init) {
            heap.add_extent_live();
            return Ok(Some(ptr));
        }
        self.unmap(extent, pages)?;
        Ok(None)
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

    /// After free/accept: Keep/Discard retain published in cache; Unmap / over-budget unpublish.
    pub(crate) fn cache_or_unmap(
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
        self.unmapped.push_front(extent);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, ptr::write_bytes};

    use crate::{
        config::{AllocatorConfig, Budget, Hints},
        heap::extent::config::{ExtentConfig, ExtentPolicy},
        heap::{Extent, Heap, HeapId},
        layout::LayoutSpec,
        memory::{PageMap, PageOwner},
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

    fn reusable_extent() -> Extent {
        let spec = layout_spec(65_536, 8);
        let len = spec.mapping_len(Os::page_size()).unwrap();
        let mapping = Os::map(len).unwrap();

        Extent::new(&OWNER, mapping, spec).unwrap()
    }

    #[test]
    fn failed_extent_page_publication_keeps_immortal_slot_unmapped() {
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
        let pages = PageMap::new();
        let index = heap.extents.vacant().unwrap();
        let inserted = heap.extents.insert(index, reusable_extent()).unwrap();
        // SAFETY: the test arena keeps this slot for the test's lifetime.
        let slot = unsafe { &*core::ptr::from_mut(inserted).cast_const() };
        let base = slot.mapping().range().base();
        // Occupy the pages first; publication must fail closed.
        pages.publish(PageOwner::Extent(slot)).unwrap();

        assert!(heap.publish(slot, &pages).is_none());

        // Slots are immortal: only the mapping is dropped.
        assert!(heap.extents.get(index).is_some());
        assert!(heap.extents.get(index).unwrap().take_mapping().is_none());
        assert_eq!(pages.get(base), Some(PageOwner::Extent(slot)));
    }

    #[test]
    fn keep_free_leaves_page_map_entry_published() {
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
        let mut heap = ExtentHeap::new(
            ExtentConfig::new().with_budget(Budget::new(2, 64 * 1024 * 1024)),
            Hints::new(),
        );
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
        let len = spec.mapping_len(Os::page_size()).unwrap();
        let mut heap = ExtentHeap::new(
            ExtentConfig::new().with_budget(Budget::new(64, 2 * len)),
            Hints::new(),
        );
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
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
        let mut heap = ExtentHeap::new(
            ExtentConfig::new().with_policy(ExtentPolicy::Unmap),
            Hints::new(),
        );
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
    fn zeroed_allocate_clears_cached_mapping() {
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
        let mut heap = ExtentHeap::new(
            ExtentConfig::new().with_policy(ExtentPolicy::Discard),
            Hints::new(),
        );
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
        let mut heap = ExtentHeap::new(ExtentConfig::new(), Hints::new());
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
