use core::mem::MaybeUninit;

use crate::{
    config::{ExtentConfig, ExtentPolicy, ExtentReuse},
    memory::Mapping,
};

pub(crate) struct ExtentCache {
    slots: [ExtentSlot; Self::MAX_SLOTS],
    retained_bytes: usize,
    config: ExtentConfig,
    epoch: u64,
}

impl ExtentCache {
    const MAX_SLOTS: usize = 64;

    pub(crate) const fn new(config: ExtentConfig) -> Self {
        Self {
            slots: [const { ExtentSlot::empty() }; Self::MAX_SLOTS],
            retained_bytes: 0,
            config,
            epoch: 0,
        }
    }

    pub(crate) fn take(&mut self, len: usize) -> Option<Mapping> {
        let index = match self.config.reuse() {
            ExtentReuse::Exact => self.find_exact(len),
            ExtentReuse::BestFit => self.find_best_fit(len),
            ExtentReuse::SizeClass => self.find_size_class(len),
        }?;

        let slot = self.slots.get_mut(index)?;
        let mapping = slot.take()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(mapping.range().len());

        Some(mapping)
    }

    pub(crate) fn will_retain(&self, len: usize) -> bool {
        let budget = self.config.budget();

        if self.config.policy() == ExtentPolicy::Drop || budget.slots() == 0 || budget.bytes() < len
        {
            return false;
        }

        match self.config.policy() {
            ExtentPolicy::Drop => false,
            ExtentPolicy::Keep => {
                self.has_empty_slot() && self.retained_bytes <= budget.bytes() - len
            }
            ExtentPolicy::Fifo
            | ExtentPolicy::Lifo
            | ExtentPolicy::Largest
            | ExtentPolicy::Smallest => true,
        }
    }

    pub(crate) fn insert(&mut self, mapping: Mapping) -> Result<(), Mapping> {
        let len = mapping.range().len();

        if !self.will_retain(len) {
            return Err(mapping);
        }

        while !self.can_fit(len) {
            let Some(index) = self.evict_index() else {
                return Err(mapping);
            };
            self.evict(index);
        }

        let Some(index) = self.empty_slot_index() else {
            return Err(mapping);
        };

        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(mapping);
        };

        self.epoch = self.epoch.wrapping_add(1);
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(mapping);
        };
        slot.insert(mapping, self.epoch);
        self.retained_bytes = retained_bytes;

        Ok(())
    }

    fn can_fit(&self, len: usize) -> bool {
        self.has_empty_slot()
            && self
                .retained_bytes
                .checked_add(len)
                .is_some_and(|bytes| bytes <= self.config.budget().bytes())
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

    fn find_best_fit(&self, len: usize) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let slot_len = slot.len()?;
                (slot_len >= len).then_some((index, slot_len))
            })
            .min_by_key(|&(_, slot_len)| slot_len)
            .map(|(index, _)| index)
    }

    fn find_size_class(&self, len: usize) -> Option<usize> {
        let class = size_class_len(len)?;

        self.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let slot_len = slot.len()?;
                (size_class_len(slot_len) == Some(class) && slot_len >= len)
                    .then_some((index, slot_len))
            })
            .min_by_key(|&(_, slot_len)| slot_len)
            .map(|(index, _)| index)
    }

    fn evict_index(&self) -> Option<usize> {
        match self.config.policy() {
            ExtentPolicy::Drop | ExtentPolicy::Keep => None,
            ExtentPolicy::Fifo => self.oldest_index(),
            ExtentPolicy::Lifo => self.newest_index(),
            ExtentPolicy::Largest => self.largest_index(),
            ExtentPolicy::Smallest => self.smallest_index(),
        }
    }

    fn oldest_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_occupied())
            .min_by_key(|(_, slot)| slot.epoch())
            .map(|(index, _)| index)
    }

    fn newest_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_occupied())
            .max_by_key(|(_, slot)| slot.epoch())
            .map(|(index, _)| index)
    }

    fn largest_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.len().map(|len| (index, len)))
            .max_by_key(|&(_, len)| len)
            .map(|(index, _)| index)
    }

    fn smallest_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.len().map(|len| (index, len)))
            .min_by_key(|&(_, len)| len)
            .map(|(index, _)| index)
    }

    fn evict(&mut self, index: usize) {
        if let Some(mapping) = self.slots.get_mut(index).and_then(ExtentSlot::take) {
            self.retained_bytes = self.retained_bytes.saturating_sub(mapping.range().len());
            drop(mapping);
        }
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
    epoch: u64,
}

impl ExtentSlot {
    const fn empty() -> Self {
        Self {
            mapping: MaybeUninit::uninit(),
            occupied: false,
            epoch: 0,
        }
    }

    const fn is_empty(&self) -> bool {
        !self.occupied
    }

    const fn is_occupied(&self) -> bool {
        self.occupied
    }

    const fn epoch(&self) -> u64 {
        self.epoch
    }

    fn len(&self) -> Option<usize> {
        if self.is_empty() {
            return None;
        }

        // SAFETY: occupied is set only after mapping.write initializes the slot.
        Some(unsafe { self.mapping.assume_init_ref() }.range().len())
    }

    fn insert(&mut self, mapping: Mapping, epoch: u64) {
        debug_assert!(self.is_empty());

        self.mapping.write(mapping);
        self.epoch = epoch;
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

fn size_class_len(len: usize) -> Option<usize> {
    len.checked_next_power_of_two()
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{Budget, ExtentConfig},
        memory::OsMemory,
    };

    use super::*;

    fn cache(policy: ExtentPolicy, budget: Budget, reuse: ExtentReuse) -> ExtentCache {
        ExtentCache::new(
            ExtentConfig::new()
                .with_policy(policy)
                .with_budget(budget)
                .with_reuse(reuse),
        )
    }

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
        let mut cache = cache(
            ExtentPolicy::Keep,
            Budget::new(2, 1024 * 1024),
            ExtentReuse::Exact,
        );

        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn extent_cache_enforces_byte_capacity_for_keep_policy() {
        let mut cache = cache(ExtentPolicy::Keep, Budget::new(4, 4096), ExtentReuse::Exact);

        assert!(cache.insert(mapping(4096)).is_ok());
        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn extent_cache_drop_policy_retains_nothing() {
        let mut cache = cache(
            ExtentPolicy::Drop,
            Budget::new(32, 1024 * 1024),
            ExtentReuse::Exact,
        );

        assert!(cache.insert(mapping(4096)).is_err());
        assert!(cache.take(4096).is_none());
    }

    #[test]
    fn extent_cache_fifo_evicts_oldest_mapping() {
        let mut cache = cache(ExtentPolicy::Fifo, Budget::new(2, 8192), ExtentReuse::Exact);
        let first = mapping(4096);
        let first_ptr = first.base();
        let second = mapping(4096);
        let second_ptr = second.base();
        let third = mapping(4096);
        let third_ptr = third.base();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(second).is_ok());
        assert!(cache.insert(third).is_ok());

        let reused = cache.take(4096).unwrap();
        assert_ne!(reused.base(), first_ptr);
        assert!(reused.base() == second_ptr || reused.base() == third_ptr);
    }

    #[test]
    fn extent_cache_lifo_evicts_newest_mapping() {
        let mut cache = cache(ExtentPolicy::Lifo, Budget::new(2, 8192), ExtentReuse::Exact);
        let first = mapping(4096);
        let first_ptr = first.base();
        let second = mapping(4096);
        let second_ptr = second.base();
        let third = mapping(4096);

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(second).is_ok());
        assert!(cache.insert(third).is_ok());

        let first_reused = cache.take(4096).unwrap().base();
        let second_reused = cache.take(4096).unwrap().base();

        assert!(first_reused == first_ptr || second_reused == first_ptr);
        assert_ne!(first_reused, second_ptr);
        assert_ne!(second_reused, second_ptr);
    }

    #[test]
    fn extent_cache_best_fit_reuses_smallest_sufficient_mapping() {
        let mut cache = cache(
            ExtentPolicy::Keep,
            Budget::new(4, 1024 * 1024),
            ExtentReuse::BestFit,
        );
        let large = mapping(512 * 1024);
        let small = mapping(256 * 1024);
        let small_ptr = small.base();

        assert!(cache.insert(large).is_ok());
        assert!(cache.insert(small).is_ok());

        let reused = cache.take(128 * 1024).unwrap();
        assert_eq!(reused.base(), small_ptr);
    }

    #[test]
    fn extent_cache_size_class_reuses_mapping_in_requested_class() {
        let mut cache = cache(
            ExtentPolicy::Keep,
            Budget::new(4, 1024 * 1024),
            ExtentReuse::SizeClass,
        );
        let matching = mapping(256 * 1024);
        let matching_ptr = matching.base();

        assert!(cache.insert(mapping(512 * 1024)).is_ok());
        assert!(cache.insert(matching).is_ok());

        let reused = cache.take(128 * 1024 + 1).unwrap();
        assert_eq!(reused.base(), matching_ptr);
    }
}
