use core::mem::MaybeUninit;

use crate::{
    config::{ExtentConfig, ExtentPolicy},
    memory::Mapping,
};

/// Bounded cache of retained extent mappings.
///
/// Reuse is exact-length only: a retained mapping is returned only if it
/// matches the requested length exactly. `ExtentPolicy::Keep` admits a freed
/// mapping while slot and byte budgets allow it and never evicts an already
/// retained mapping to make room; `ExtentPolicy::Drop` retains nothing.
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

    pub(crate) fn take(&mut self, len: usize) -> Option<Mapping> {
        let index = self.find_exact(len)?;
        let slot = self.slots.get_mut(index)?;
        let mapping = slot.take()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(mapping.range().len());

        Some(mapping)
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

    pub(crate) fn insert(&mut self, mapping: Mapping) -> Result<(), Mapping> {
        let len = mapping.range().len();

        if !self.will_retain(len) {
            return Err(mapping);
        }

        let Some(index) = self.empty_slot_index() else {
            return Err(mapping);
        };

        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(mapping);
        };

        let Some(slot) = self.slots.get_mut(index) else {
            return Err(mapping);
        };
        slot.insert(mapping);
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
        for slot in &mut self.slots {
            let _ = slot.take();
        }
    }
}

struct ExtentSlot {
    mapping: MaybeUninit<Mapping>,
    occupied: bool,
}

impl ExtentSlot {
    const fn empty() -> Self {
        Self {
            mapping: MaybeUninit::uninit(),
            occupied: false,
        }
    }

    const fn is_empty(&self) -> bool {
        !self.occupied
    }

    fn len(&self) -> Option<usize> {
        if self.is_empty() {
            return None;
        }

        // SAFETY: occupied is set only after mapping.write initializes the slot.
        Some(unsafe { self.mapping.assume_init_ref() }.range().len())
    }

    fn insert(&mut self, mapping: Mapping) {
        debug_assert!(self.is_empty());

        self.mapping.write(mapping);
        self.occupied = true;
    }

    fn take(&mut self) -> Option<Mapping> {
        if self.is_empty() {
            return None;
        }

        self.occupied = false;

        // SAFETY: occupied was true on entry, so mapping is initialized.
        Some(unsafe { self.mapping.assume_init_read() })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{Budget, ExtentConfig, ExtentPolicy},
        memory::OsMemory,
    };

    use super::*;

    fn mapping(len: usize) -> Mapping {
        OsMemory::map(len).unwrap()
    }

    #[test]
    fn extent_cache_reuses_exact_length() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let mapping = mapping(256 * 1024);
        let len = mapping.range().len();
        let ptr = mapping.base();

        assert!(cache.insert(mapping).is_ok());

        let reused = cache.take(len).unwrap();
        assert_eq!(reused.base(), ptr);
    }

    #[test]
    fn extent_cache_rejects_nonmatching_exact_lookup() {
        let mut cache = ExtentCache::new(ExtentConfig::new());

        assert!(cache.insert(mapping(256 * 1024)).is_ok());
        assert!(cache.take(128 * 1024).is_none());
    }

    #[test]
    fn extent_cache_enforces_slot_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(2, 1024 * 1024)),
        );

        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn extent_cache_enforces_byte_capacity_for_keep_policy() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(4, 4096)),
        );

        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn extent_cache_drop_policy_retains_nothing() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Drop)
                .with_budget(Budget::new(32, 1024 * 1024)),
        );

        assert!(cache.insert(mapping(4096)).is_err());
        assert!(cache.take(4096).is_none());
    }

    #[test]
    fn extent_cache_keep_policy_never_evicts_to_make_room() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(1, 8192)),
        );
        let first = mapping(4096);
        let first_ptr = first.base();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(mapping(4096)).is_err());

        let reused = cache.take(4096).unwrap();
        assert_eq!(reused.base(), first_ptr);
    }
}
