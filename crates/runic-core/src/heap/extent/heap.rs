use core::{
    ptr::{NonNull, write_bytes},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    arena::Arena,
    heap::extent::config::ExtentConfig,
    heap::{Extent, Heap, HeapError},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
};

use super::{ExtentId, cache::ExtentCache};

/// Zeroed Keep reuse at or above this size discards instead of memset.
const LAZY_ZERO: usize = 64 * 1024;

pub(crate) struct ExtentHeap {
    /// Allocated/claimed extents. Cached Free extents are not live.
    live: AtomicUsize,
    extents: Arena<Extent>,
    cache: ExtentCache,
}

/// How a newly allocated extent's bytes should be initialized.
///
/// Fresh anonymous mappings are already kernel-zeroed. Cached extents may be
/// dirty, so [`ExtentInit::Zeroed`] zeros on cache hits: Discard-insert already
/// dropped the pages, else Keep discards when `size ≥ 64 KiB` or memsets.
/// Allocate-time Keep discard does not set the cache-clean flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentInit {
    Uninit,
    Zeroed,
}

// SAFETY: ExtentHeap owns extent metadata and cache pointers into its own
// arena. Moving the heap to another thread does not permit concurrent mutation;
// exclusive access stays under HeapInner.
unsafe impl Send for ExtentHeap {}

impl ExtentHeap {
    pub(crate) fn new(config: ExtentConfig) -> Self {
        Self {
            live: AtomicUsize::new(0),
            extents: Arena::new(),
            cache: ExtentCache::new(config),
        }
    }

    fn add_live(&self) {
        self.live.fetch_add(1, Ordering::Release);
    }

    fn sub_live(&self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn occupied(&self) -> bool {
        self.live.load(Ordering::Acquire) != 0
    }

    /// Any occupied extent that is still Allocated or Claimed.
    ///
    /// Production reclaim uses [`Self::occupied`] then this scan.
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
    ) -> Option<NonNull<u8>> {
        let len = spec.mapping_len(OsMemory::page_size())?;
        if let Some(mut extent_ptr) = self.cache.acquire(len) {
            // SAFETY: cache only stores live arena extents owned by this heap.
            let extent = unsafe { extent_ptr.as_mut() };
            let cache_clean = extent.discarded();
            if let Some(ptr) = extent.reuse(spec) {
                if init == ExtentInit::Zeroed && !cache_clean {
                    let zeroed =
                        spec.size() >= LAZY_ZERO && OsMemory::discard(extent.mapping().range());
                    if !zeroed {
                        // SAFETY: ptr was just reused for spec and is valid for spec.size() bytes.
                        unsafe { write_bytes(ptr.as_ptr(), 0, spec.size()) };
                    }
                }
                self.add_live();
                return Some(ptr);
            }
            // Cache keyed by mapping length; reuse failure is rare (align) — release and remap.
            let _ = self.unmap(extent_ptr, pages);
        }

        let mapping = OsMemory::map(len)?;
        let ptr = self.allocate_mapping(spec, heap, mapping, pages)?;
        self.add_live();
        Some(ptr)
    }

    fn allocate_mapping(
        &mut self,
        spec: LayoutSpec,
        heap: &'static Heap,
        mapping: crate::memory::Mapping,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let index = self.extents.vacant()?;
        let id = ExtentId::from_index(index)?;
        let extent = Extent::new(id, heap, mapping, spec)?;
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

    /// After free/accept: Keep/Discard retain published in cache; Unmap / over-budget unpublish.
    fn cache_or_unmap(
        &mut self,
        extent_ptr: NonNull<Extent>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        debug_assert!(!unsafe { extent_ptr.as_ref() }.is_live());
        self.sub_live();
        if self.cache.insert(extent_ptr).is_ok() {
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
        let inserted_extent = self.extents.insert(index, extent)?;
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
    use std::sync::OnceLock;

    use crate::{
        config::AllocatorConfig,
        heap::extent::config::{ExtentConfig, ExtentPolicy},
        heap::{Extent, Heap, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
    };

    use super::*;

    static OWNER: OnceLock<Heap> = OnceLock::new();

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = layout_spec(65_536, 8);
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(
            id,
            OWNER.get_or_init(|| {
                Heap::new(
                    HeapId::new(0, NonZeroU32::MIN).unwrap(),
                    AllocatorConfig::new(),
                )
            }),
            mapping,
            spec,
        )
        .unwrap()
    }

    #[test]
    fn failed_extent_page_publication_removes_map_entry() {
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let index = heap.extents.vacant().unwrap();
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
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let ptr = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
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
        let mut heap = ExtentHeap::new(ExtentConfig::new());
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let first = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
            .unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
            .unwrap();
        assert_eq!(reused, first);
        assert_eq!(pages.get(reused), Some(PageOwner::Extent(extent)));
    }

    #[test]
    fn unmap_policy_unpublishes_on_free() {
        let mut heap = ExtentHeap::new(ExtentConfig::new().with_policy(ExtentPolicy::Unmap));
        let pages = PageMap::new();
        let spec = layout_spec(128 * 1024, 4096);
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let ptr = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
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
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let ptr = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
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
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let first = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
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
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let first = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
            .unwrap();
        // SAFETY: first is valid for LAZY_ZERO bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, LAZY_ZERO) };
        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let second = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
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
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
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
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let first = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xab, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();
        assert_eq!(pages.get(first), Some(PageOwner::Extent(extent)));

        let reused = heap
            .allocate(spec, owner, &pages, ExtentInit::Zeroed)
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
        let owner = OWNER.get_or_init(|| {
            Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )
        });

        let first = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
            .unwrap();
        // SAFETY: first is valid for size bytes.
        unsafe { write_bytes(first.as_ptr(), 0xcd, size) };

        let Some(PageOwner::Extent(extent)) = pages.get(first) else {
            panic!("expected extent owner");
        };
        heap.free(extent, first, &pages).unwrap();

        let reused = heap
            .allocate(spec, owner, &pages, ExtentInit::Uninit)
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
