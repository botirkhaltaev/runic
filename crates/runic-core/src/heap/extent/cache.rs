use core::ptr::NonNull;

use crate::{
    config::{ExtentConfig, ExtentPolicy},
    heap::Extent,
};

/// Bounded index of retained published extents.
///
/// Slots hold [`NonNull<Extent>`] into the owning [`super::heap::ExtentHeap`] arena.
/// Cached extents stay page-map published; reuse is exact mapping-length only.
/// `ExtentPolicy::Keep` admits while slot and byte budgets allow and never evicts an
/// already retained extent to make room; `ExtentPolicy::Drop` retains nothing.
pub(crate) struct ExtentCache {
    slots: [ExtentSlot; Self::MAX_SLOTS],
    retained_bytes: usize,
    config: ExtentConfig,
}

impl ExtentCache {
    const MAX_SLOTS: usize = 64;

    pub(crate) const fn new(config: ExtentConfig) -> Self {
        Self {
            slots: [const { ExtentSlot::empty() }; Self::MAX_SLOTS],
            retained_bytes: 0,
            config,
        }
    }

    pub(crate) fn take(&mut self, len: usize) -> Option<NonNull<Extent>> {
        let index = self.find_exact(len)?;
        let slot = self.slots.get_mut(index)?;
        let extent = slot.take()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(len);

        Some(extent)
    }

    pub(crate) fn will_retain(&self, len: usize) -> bool {
        if self.config.policy() == ExtentPolicy::Drop {
            return false;
        }

        let budget = self.config.budget();
        budget.slots() != 0
            && budget.bytes() >= len
            && self.has_empty_slot()
            && self.retained_bytes <= budget.bytes() - len
    }

    pub(crate) fn insert(&mut self, extent: NonNull<Extent>) -> Result<(), NonNull<Extent>> {
        // SAFETY: caller only inserts arena extents owned by the parent ExtentHeap.
        let len = unsafe { extent.as_ref() }.mapping().len().get();

        if !self.will_retain(len) {
            return Err(extent);
        }

        let Some(index) = self.empty_slot_index() else {
            return Err(extent);
        };

        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(extent);
        };

        let Some(slot) = self.slots.get_mut(index) else {
            return Err(extent);
        };
        slot.insert(extent, len);
        self.retained_bytes = retained_bytes;

        Ok(())
    }

    fn active_slots(&self) -> usize {
        self.config.budget().slots().min(Self::MAX_SLOTS)
    }

    fn slots(&self) -> &[ExtentSlot] {
        let active = self.active_slots();
        self.slots.get(..active).unwrap_or(&[])
    }

    fn has_empty_slot(&self) -> bool {
        self.slots().iter().any(ExtentSlot::is_empty)
    }

    fn empty_slot_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .find_map(|(index, slot)| slot.is_empty().then_some(index))
    }

    fn find_exact(&self, len: usize) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .find_map(|(index, slot)| (slot.len() == Some(len)).then_some(index))
    }
}

impl Drop for ExtentCache {
    fn drop(&mut self) {
        // Index only — arena/`Extent` owns the mappings and munmaps them.
        for slot in &mut self.slots {
            let _ = slot.take();
        }
    }
}

struct ExtentSlot {
    extent: Option<NonNull<Extent>>,
    len: usize,
}

impl ExtentSlot {
    const fn empty() -> Self {
        Self {
            extent: None,
            len: 0,
        }
    }

    const fn is_empty(&self) -> bool {
        self.extent.is_none()
    }

    const fn len(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else {
            Some(self.len)
        }
    }

    fn insert(&mut self, extent: NonNull<Extent>, len: usize) {
        debug_assert!(self.is_empty());
        self.extent = Some(extent);
        self.len = len;
    }

    fn take(&mut self) -> Option<NonNull<Extent>> {
        self.len = 0;
        self.extent.take()
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, num::NonZeroU32};

    use crate::{
        config::{Budget, ExtentConfig, ExtentPolicy},
        heap::{Extent, HeapId, extent::ExtentId},
        layout::LayoutSpec,
        memory::OsMemory,
    };

    use super::*;

    fn heap_id() -> HeapId {
        HeapId::new(0, NonZeroU32::MIN).unwrap()
    }

    fn leaked_free_extent(mapping_len: usize) -> NonNull<Extent> {
        let spec = LayoutSpec::from_layout(Layout::from_size_align(mapping_len, 8).unwrap());
        let mapping = OsMemory::map(mapping_len).unwrap();
        let extent =
            Extent::new(ExtentId::from_index(0).unwrap(), heap_id(), mapping, spec).unwrap();
        assert_eq!(extent.free(extent.ptr()), Ok(()));
        NonNull::from(Box::leak(Box::new(extent)))
    }

    #[test]
    fn extent_cache_reuses_exact_length() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let extent = leaked_free_extent(256 * 1024);
        // SAFETY: test-owned leaked extent.
        let ptr = unsafe { extent.as_ref() }.ptr();
        let len = unsafe { extent.as_ref() }.mapping().len().get();

        assert!(cache.insert(extent).is_ok());

        let reused = cache.take(len).unwrap();
        // SAFETY: returned from cache; still the leaked extent.
        assert_eq!(unsafe { reused.as_ref() }.ptr(), ptr);
    }

    #[test]
    fn extent_cache_rejects_nonmatching_exact_lookup() {
        let mut cache = ExtentCache::new(ExtentConfig::new());

        assert!(cache.insert(leaked_free_extent(256 * 1024)).is_ok());
        assert!(cache.take(128 * 1024).is_none());
    }

    #[test]
    fn extent_cache_enforces_slot_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(2, 1024 * 1024)),
        );

        assert!(cache.insert(leaked_free_extent(4096)).is_ok());
        assert!(cache.insert(leaked_free_extent(4096)).is_ok());
        assert!(cache.insert(leaked_free_extent(4096)).is_err());
    }

    #[test]
    fn extent_cache_enforces_byte_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(4, 4096)),
        );

        assert!(cache.insert(leaked_free_extent(4096)).is_ok());
        assert!(cache.insert(leaked_free_extent(4096)).is_err());
    }

    #[test]
    fn extent_cache_drop_policy_retains_nothing() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Drop)
                .with_budget(Budget::new(32, 1024 * 1024)),
        );

        assert!(cache.insert(leaked_free_extent(4096)).is_err());
        assert!(cache.take(4096).is_none());
    }

    #[test]
    fn extent_cache_keep_policy_never_evicts_to_make_room() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(1, 8192)),
        );
        let first = leaked_free_extent(4096);
        // SAFETY: test-owned leaked extent.
        let first_ptr = unsafe { first.as_ref() }.ptr();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(leaked_free_extent(4096)).is_err());

        let reused = cache.take(4096).unwrap();
        // SAFETY: returned from cache; still the leaked extent.
        assert_eq!(unsafe { reused.as_ref() }.ptr(), first_ptr);
    }
}
