use core::mem::MaybeUninit;

use crate::{
    config::{RunConfig, RunPolicy},
    memory::Mapping,
    run::RUN_SIZE,
    size_class::SizeClassId,
};

pub(crate) struct RunCache {
    slots: [RunSlot; Self::MAX_SLOTS],
    retained_bytes: usize,
    config: RunConfig,
    epoch: u64,
}

impl RunCache {
    const MAX_SLOTS: usize = 64;

    pub(crate) const fn new(config: RunConfig) -> Self {
        Self {
            slots: [const { RunSlot::empty() }; Self::MAX_SLOTS],
            retained_bytes: 0,
            config,
            epoch: 0,
        }
    }

    pub(crate) fn take(&mut self, class: SizeClassId) -> Option<Mapping> {
        let index = match self.config.policy() {
            RunPolicy::Keep | RunPolicy::DropEmpty => None,
            RunPolicy::RetainFifo => self.oldest_index(),
            RunPolicy::RetainPerClass => {
                self.same_class_index(class).or_else(|| self.oldest_index())
            }
        }?;

        let mapping = self.slots.get_mut(index)?.take()?;
        self.retained_bytes = self.retained_bytes.saturating_sub(RUN_SIZE);

        Some(mapping)
    }

    pub(crate) fn will_retain(&self) -> bool {
        let budget = self.config.budget();

        self.config.retains_empty_runs() && budget.slots() != 0 && budget.bytes() >= RUN_SIZE
    }

    pub(crate) fn insert(&mut self, mapping: Mapping, class: SizeClassId) -> Result<(), Mapping> {
        if !self.will_retain() {
            return Err(mapping);
        }

        while !self.can_fit() {
            let Some(index) = self.evict_index() else {
                return Err(mapping);
            };
            self.evict(index);
        }

        let Some(index) = self.empty_slot_index() else {
            return Err(mapping);
        };

        let Some(retained_bytes) = self.retained_bytes.checked_add(RUN_SIZE) else {
            return Err(mapping);
        };

        self.epoch = self.epoch.wrapping_add(1);
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(mapping);
        };
        slot.insert(mapping, class, self.epoch);
        self.retained_bytes = retained_bytes;

        Ok(())
    }

    fn can_fit(&self) -> bool {
        self.has_empty_slot()
            && self
                .retained_bytes
                .checked_add(RUN_SIZE)
                .is_some_and(|bytes| bytes <= self.config.budget().bytes())
    }

    fn active_slots(&self) -> usize {
        self.config.budget().slots().min(Self::MAX_SLOTS)
    }

    fn slots(&self) -> &[RunSlot] {
        let active = self.active_slots();
        self.slots.get(..active).unwrap_or(&[])
    }

    fn has_empty_slot(&self) -> bool {
        self.slots().iter().any(RunSlot::is_empty)
    }

    fn empty_slot_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .find_map(|(index, slot)| slot.is_empty().then_some(index))
    }

    fn same_class_index(&self, class: SizeClassId) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.class() == Some(class))
            .min_by_key(|(_, slot)| slot.epoch())
            .map(|(index, _)| index)
    }

    fn oldest_index(&self) -> Option<usize> {
        self.slots()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_occupied())
            .min_by_key(|(_, slot)| slot.epoch())
            .map(|(index, _)| index)
    }

    fn evict_index(&self) -> Option<usize> {
        match self.config.policy() {
            RunPolicy::Keep | RunPolicy::DropEmpty => None,
            RunPolicy::RetainFifo | RunPolicy::RetainPerClass => self.oldest_index(),
        }
    }

    fn evict(&mut self, index: usize) {
        if let Some(mapping) = self.slots.get_mut(index).and_then(RunSlot::take) {
            self.retained_bytes = self.retained_bytes.saturating_sub(RUN_SIZE);
            drop(mapping);
        }
    }
}

impl Drop for RunCache {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            let _ = slot.take();
        }
    }
}

struct RunSlot {
    mapping: MaybeUninit<Mapping>,
    occupied: bool,
    class: Option<SizeClassId>,
    epoch: u64,
}

impl RunSlot {
    const fn empty() -> Self {
        Self {
            mapping: MaybeUninit::uninit(),
            occupied: false,
            class: None,
            epoch: 0,
        }
    }

    const fn is_empty(&self) -> bool {
        !self.occupied
    }

    const fn is_occupied(&self) -> bool {
        self.occupied
    }

    const fn class(&self) -> Option<SizeClassId> {
        self.class
    }

    const fn epoch(&self) -> u64 {
        self.epoch
    }

    fn insert(&mut self, mapping: Mapping, class: SizeClassId, epoch: u64) {
        debug_assert!(self.is_empty());

        self.mapping.write(mapping);
        self.class = Some(class);
        self.epoch = epoch;
        self.occupied = true;
    }

    fn take(&mut self) -> Option<Mapping> {
        if self.is_empty() {
            return None;
        }

        self.occupied = false;
        self.class = None;

        // SAFETY: occupied was true on entry, so mapping is initialized.
        Some(unsafe { self.mapping.assume_init_read() })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{Budget, RunConfig},
        memory::OsMemory,
        size_class::SizeClasses,
    };

    use super::*;

    fn class(size: usize) -> SizeClassId {
        let spec = crate::layout::LayoutSpec::from_size_align(size, 8).unwrap();
        SizeClasses::for_layout(spec).unwrap().id()
    }

    fn cache(policy: RunPolicy, slots: usize) -> RunCache {
        RunCache::new(
            RunConfig::new()
                .with_policy(policy)
                .with_budget(Budget::new(slots, slots * RUN_SIZE)),
        )
    }

    fn mapping() -> Mapping {
        OsMemory::map(RUN_SIZE).unwrap()
    }

    #[test]
    fn run_cache_keep_policy_retains_nothing() {
        let mut cache = cache(RunPolicy::Keep, 1);

        assert!(cache.insert(mapping(), class(64)).is_err());
        assert!(cache.take(class(64)).is_none());
    }

    #[test]
    fn run_cache_reuses_retained_mapping() {
        let mut cache = cache(RunPolicy::RetainFifo, 1);
        let mapping = mapping();
        let ptr = mapping.base();

        assert!(cache.insert(mapping, class(64)).is_ok());

        let reused = cache.take(class(128)).unwrap();
        assert_eq!(reused.base(), ptr);
    }

    #[test]
    fn run_cache_enforces_slot_capacity_by_eviction() {
        let mut cache = cache(RunPolicy::RetainFifo, 1);
        let first = mapping();
        let first_ptr = first.base();
        let second = mapping();
        let second_ptr = second.base();

        assert!(cache.insert(first, class(64)).is_ok());
        assert!(cache.insert(second, class(64)).is_ok());

        let reused = cache.take(class(64)).unwrap();
        assert_ne!(reused.base(), first_ptr);
        assert_eq!(reused.base(), second_ptr);
    }

    #[test]
    fn run_cache_per_class_prefers_matching_class() {
        let mut cache = cache(RunPolicy::RetainPerClass, 2);
        let small = mapping();
        let large = mapping();
        let large_ptr = large.base();

        assert!(cache.insert(small, class(64)).is_ok());
        assert!(cache.insert(large, class(128)).is_ok());

        let reused = cache.take(class(128)).unwrap();
        assert_eq!(reused.base(), large_ptr);
    }
}
