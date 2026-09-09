use core::ptr::NonNull;

use crate::{
    heap::Extent,
    heap::extent::config::{ExtentConfig, ExtentPolicy},
};

/// Intrusive list of retained published extents.
///
/// Links are [`Extent::next`] into the owning [`super::heap::ExtentHeap`] arena.
/// Cached extents stay page-map published; reuse is exact mapping-length only.
/// `ExtentPolicy::{Keep, Discard}` admit while slot and byte budgets allow and never
/// evict an already retained extent to make room; `ExtentPolicy::Unmap` retains nothing.
pub(crate) struct ExtentCache {
    head: Option<NonNull<Extent>>,
    count: usize,
    retained_bytes: usize,
    config: ExtentConfig,
}

impl ExtentCache {
    pub(crate) const fn new(config: ExtentConfig) -> Self {
        Self {
            head: None,
            count: 0,
            retained_bytes: 0,
            config,
        }
    }

    pub(crate) fn acquire(&mut self, len: usize) -> Option<NonNull<Extent>> {
        let mut prev = None;
        let mut found = None;
        // SAFETY: cache only stores live arena extents owned by the parent ExtentHeap.
        for extent in unsafe { self.head?.as_ref() }.iter() {
            if extent.mapping().len().get() == len {
                found = Some((prev, NonNull::from_ref(extent), extent.next()));
                break;
            }
            prev = Some(NonNull::from_ref(extent));
        }
        let (prev, mut extent, next) = found?;
        if let Some(mut prev) = prev {
            // SAFETY: prev is an earlier cache node in this list.
            unsafe { prev.as_mut() }.set_next(next);
        } else {
            self.head = next;
        }
        // SAFETY: unlinking this node; iterator borrow ended at break.
        unsafe { extent.as_mut() }.set_next(None);
        debug_assert!(self.count >= 1);
        debug_assert!(self.retained_bytes >= len);
        self.count -= 1;
        self.retained_bytes -= len;
        Some(extent)
    }

    fn will_retain(&self, len: usize) -> bool {
        if !self.config.policy().retains() {
            return false;
        }

        let budget = self.config.budget();
        budget.slots() != 0
            && self.count < budget.slots()
            && budget.bytes() >= len
            && self.retained_bytes <= budget.bytes() - len
    }

    pub(crate) fn insert(&mut self, mut extent: NonNull<Extent>) -> Result<(), NonNull<Extent>> {
        // SAFETY: caller only inserts arena extents owned by the parent ExtentHeap.
        let len = unsafe { extent.as_ref() }.mapping().len().get();

        if !self.will_retain(len) {
            return Err(extent);
        }

        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(extent);
        };

        // SAFETY: owner-exclusive insert onto the cache list.
        unsafe { extent.as_mut() }.set_next(self.head);
        self.head = Some(extent);
        self.count += 1;
        self.retained_bytes = retained_bytes;
        if self.config.policy() == ExtentPolicy::Discard {
            // SAFETY: just cached; this heap is the exclusive owner.
            unsafe { extent.as_mut() }.discard();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, num::NonZeroU32};

    use crate::{
        config::Budget,
        heap::extent::config::{ExtentConfig, ExtentPolicy},
        heap::{Extent, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::OsMemory,
    };

    use super::*;

    /// Owns heap-allocated Free extents for cache tests; drops after the cache.
    struct OwnedExtents {
        extents: Vec<NonNull<Extent>>,
    }

    impl OwnedExtents {
        fn new() -> Self {
            Self {
                extents: Vec::new(),
            }
        }

        fn free_extent(&mut self, mapping_len: usize) -> NonNull<Extent> {
            let heap_id = HeapId::new(0, NonZeroU32::MIN).unwrap();
            let spec = LayoutSpec::from_layout(Layout::from_size_align(mapping_len, 8).unwrap());
            let mapping = OsMemory::map(mapping_len).unwrap();
            let extent =
                Extent::new(ExtentId::from_index(0).unwrap(), heap_id, mapping, spec).unwrap();
            assert_eq!(extent.free(extent.ptr()), Ok(()));
            let ptr = NonNull::from(Box::leak(Box::new(extent)));
            self.extents.push(ptr);
            ptr
        }
    }

    impl Drop for OwnedExtents {
        fn drop(&mut self) {
            for ptr in self.extents.drain(..) {
                // SAFETY: each pointer came from Box::leak in free_extent; cache only indexes.
                drop(unsafe { Box::from_raw(ptr.as_ptr()) });
            }
        }
    }

    #[test]
    fn extent_cache_reuses_exact_length() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let mut owned = OwnedExtents::new();
        let extent = owned.free_extent(256 * 1024);
        // SAFETY: owned fixture extent.
        let ptr = unsafe { extent.as_ref() }.ptr();
        let len = unsafe { extent.as_ref() }.mapping().len().get();

        assert!(cache.insert(extent).is_ok());

        let reused = cache.acquire(len).unwrap();
        // SAFETY: returned from cache; still owned.
        assert_eq!(unsafe { reused.as_ref() }.ptr(), ptr);
    }

    #[test]
    fn extent_cache_rejects_nonmatching_exact_lookup() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let mut owned = OwnedExtents::new();

        assert!(cache.insert(owned.free_extent(256 * 1024)).is_ok());
        assert!(cache.acquire(128 * 1024).is_none());
    }

    #[test]
    fn extent_cache_enforces_slot_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(2, 1024 * 1024)),
        );
        let mut owned = OwnedExtents::new();

        assert!(cache.insert(owned.free_extent(4096)).is_ok());
        assert!(cache.insert(owned.free_extent(4096)).is_ok());
        assert!(cache.insert(owned.free_extent(4096)).is_err());
    }

    #[test]
    fn extent_cache_enforces_byte_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(4, 4096)),
        );
        let mut owned = OwnedExtents::new();

        assert!(cache.insert(owned.free_extent(4096)).is_ok());
        assert!(cache.insert(owned.free_extent(4096)).is_err());
    }

    #[test]
    fn extent_cache_discard_policy_retains_like_keep() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Discard)
                .with_budget(Budget::new(2, 1024 * 1024)),
        );
        let mut owned = OwnedExtents::new();

        assert!(cache.insert(owned.free_extent(4096)).is_ok());
        assert!(cache.acquire(4096).is_some());
    }

    #[test]
    fn extent_cache_unmap_policy_retains_nothing() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Unmap)
                .with_budget(Budget::new(32, 1024 * 1024)),
        );
        let mut owned = OwnedExtents::new();

        assert!(cache.insert(owned.free_extent(4096)).is_err());
        assert!(cache.acquire(4096).is_none());
    }

    #[test]
    fn extent_cache_keep_policy_never_evicts_to_make_room() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(1, 8192)),
        );
        let mut owned = OwnedExtents::new();
        let first = owned.free_extent(4096);
        // SAFETY: owned fixture extent.
        let first_ptr = unsafe { first.as_ref() }.ptr();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(owned.free_extent(4096)).is_err());

        let reused = cache.acquire(4096).unwrap();
        // SAFETY: returned from cache; still owned.
        assert_eq!(unsafe { reused.as_ref() }.ptr(), first_ptr);
    }

    #[test]
    fn extent_cache_slot_budget_above_sixty_four_is_honored() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(100, 1024 * 1024 * 1024)),
        );
        let mut owned = OwnedExtents::new();

        for _ in 0..65 {
            assert!(cache.insert(owned.free_extent(4096)).is_ok());
        }
        assert_eq!(cache.count, 65);
        assert!(cache.acquire(4096).is_some());
        assert_eq!(cache.count, 64);
    }

    #[test]
    fn extent_cache_acquire_unlinks_middle_entry() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let mut owned = OwnedExtents::new();
        let first = owned.free_extent(64 * 1024);
        let middle = owned.free_extent(128 * 1024);
        let last = owned.free_extent(256 * 1024);
        // SAFETY: owned fixture extents.
        let first_ptr = unsafe { first.as_ref() }.ptr();
        let middle_ptr = unsafe { middle.as_ref() }.ptr();
        let last_ptr = unsafe { last.as_ref() }.ptr();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(middle).is_ok());
        assert!(cache.insert(last).is_ok());

        let acquired = cache.acquire(128 * 1024).unwrap();
        // SAFETY: returned from cache; still owned.
        assert_eq!(unsafe { acquired.as_ref() }.ptr(), middle_ptr);
        assert_eq!(
            unsafe { cache.acquire(64 * 1024).unwrap().as_ref() }.ptr(),
            first_ptr
        );
        assert_eq!(
            unsafe { cache.acquire(256 * 1024).unwrap().as_ref() }.ptr(),
            last_ptr
        );
        assert!(cache.acquire(128 * 1024).is_none());
    }
}
