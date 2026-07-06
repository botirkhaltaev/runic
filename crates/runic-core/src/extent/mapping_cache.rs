use core::mem::MaybeUninit;

use crate::memory::Mapping;

pub(crate) struct MappingCache {
    slots: [ExtentMappingSlot; Self::SLOTS],
    retained_bytes: usize,
    next_epoch: u64,
    release_policy: MappingReleasePolicy,
}

impl MappingCache {
    const SLOTS: usize = 32;
    const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
    const TARGET_RETAINED_BYTES: usize = 8 * 1024 * 1024;

    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { ExtentMappingSlot::empty() }; Self::SLOTS],
            retained_bytes: 0,
            next_epoch: 1,
            release_policy: MappingReleasePolicy::new(
                Self::TARGET_RETAINED_BYTES,
                Self::MAX_RETAINED_BYTES,
            ),
        }
    }

    pub(crate) fn take_exact(&mut self, len: usize) -> Option<Mapping> {
        for slot in &mut self.slots {
            if slot.len() != Some(len) {
                continue;
            }

            let mapping = slot.take()?;
            self.retained_bytes = self.retained_bytes.saturating_sub(mapping.range().len());

            return Some(mapping);
        }

        None
    }

    pub(crate) fn can_retain(&self, len: usize) -> bool {
        self.release_policy.can_admit(self.retained_bytes, len)
            && self.slots.iter().any(ExtentMappingSlot::is_empty)
    }

    pub(crate) fn insert(&mut self, mapping: Mapping) -> Result<(), Mapping> {
        let len = mapping.range().len();
        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(mapping);
        };

        if !self.release_policy.can_hold(retained_bytes) {
            return Err(mapping);
        }

        let epoch = self.insertion_epoch();
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_empty()) else {
            return Err(mapping);
        };

        slot.insert(mapping, epoch);
        self.retained_bytes = retained_bytes;
        self.release_to(self.release_policy.target_after_insert(len));

        Ok(())
    }

    fn insertion_epoch(&mut self) -> u64 {
        if self.next_epoch == u64::MAX {
            for slot in &mut self.slots {
                slot.reset_epoch();
            }
            self.next_epoch = 1;
        }

        let epoch = self.next_epoch;
        self.next_epoch += 1;
        epoch
    }

    fn release_to(&mut self, target_bytes: usize) {
        while self.retained_bytes > target_bytes {
            let Some(index) = self.oldest_slot_index() else {
                break;
            };
            let Some(slot) = self.slots.get_mut(index) else {
                break;
            };
            let Some(mapping) = slot.take() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(mapping.range().len());
            drop(mapping);
        }
    }

    fn oldest_slot_index(&self) -> Option<usize> {
        let mut oldest = None;

        for (index, slot) in self.slots.iter().enumerate() {
            let Some(epoch) = slot.epoch() else {
                continue;
            };

            if oldest.is_none_or(|(_, oldest_epoch)| epoch < oldest_epoch) {
                oldest = Some((index, epoch));
            }
        }

        oldest.map(|(index, _)| index)
    }
}

#[derive(Clone, Copy)]
struct MappingReleasePolicy {
    target_retained_bytes: usize,
    max_retained_bytes: usize,
}

impl MappingReleasePolicy {
    const fn new(target_retained_bytes: usize, max_retained_bytes: usize) -> Self {
        Self {
            target_retained_bytes,
            max_retained_bytes,
        }
    }

    fn can_admit(self, retained_bytes: usize, len: usize) -> bool {
        retained_bytes
            .checked_add(len)
            .is_some_and(|retained_bytes| self.can_hold(retained_bytes))
    }

    const fn can_hold(self, retained_bytes: usize) -> bool {
        retained_bytes <= self.max_retained_bytes
    }

    const fn target_after_insert(self, len: usize) -> usize {
        if len > self.target_retained_bytes {
            len
        } else {
            self.target_retained_bytes
        }
    }
}

impl Drop for MappingCache {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            let _ = slot.take();
        }
    }
}

struct ExtentMappingSlot {
    mapping: MaybeUninit<Mapping>,
    occupied: bool,
    epoch: u64,
}

impl ExtentMappingSlot {
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

    fn len(&self) -> Option<usize> {
        if self.is_empty() {
            return None;
        }

        // SAFETY: occupied is set only after mapping.write initializes the slot.
        Some(unsafe { self.mapping.assume_init_ref() }.range().len())
    }

    fn epoch(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }

        Some(self.epoch)
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
        self.epoch = 0;

        // SAFETY: occupied was true on entry, so mapping is initialized.
        Some(unsafe { self.mapping.assume_init_read() })
    }

    fn reset_epoch(&mut self) {
        if self.occupied {
            self.epoch = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::OsMemory;

    use super::*;

    fn mapping(len: usize) -> Mapping {
        OsMemory::map(len).unwrap()
    }

    #[test]
    fn mapping_cache_reuses_exact_length() {
        let mut cache = MappingCache::new();
        let mapping = mapping(256 * 1024);
        let len = mapping.range().len();
        let ptr = mapping.base();

        assert!(cache.insert(mapping).is_ok());

        let reused = cache.take_exact(len).unwrap();
        assert_eq!(reused.base(), ptr);
    }

    #[test]
    fn mapping_cache_rejects_nonmatching_length_lookup() {
        let mut cache = MappingCache::new();

        assert!(cache.insert(mapping(256 * 1024)).is_ok());
        assert!(cache.take_exact(128 * 1024).is_none());
    }

    #[test]
    fn mapping_cache_enforces_slot_capacity() {
        let mut cache = MappingCache::new();

        for _ in 0..MappingCache::SLOTS {
            assert!(cache.insert(mapping(4096)).is_ok());
        }

        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn mapping_cache_enforces_byte_capacity() {
        let mut cache = MappingCache::new();

        assert!(
            cache
                .insert(mapping(MappingCache::MAX_RETAINED_BYTES))
                .is_ok()
        );
        assert!(cache.insert(mapping(4096)).is_err());
    }

    #[test]
    fn mapping_cache_decays_to_target_retained_bytes() {
        let mut cache = MappingCache::new();
        let len = 4 * 1024 * 1024;
        let first = mapping(len);
        let first_ptr = first.base();
        let second = mapping(len);
        let second_ptr = second.base();
        let third = mapping(len);
        let third_ptr = third.base();

        assert!(cache.insert(first).is_ok());
        assert!(cache.insert(second).is_ok());
        assert!(cache.insert(third).is_ok());

        assert_eq!(cache.retained_bytes, MappingCache::TARGET_RETAINED_BYTES);

        let reused = cache.take_exact(len).unwrap();
        let reused_ptr = reused.base();
        assert!(reused_ptr == second_ptr || reused_ptr == third_ptr);
        drop(reused);

        let reused = cache.take_exact(len).unwrap();
        let reused_ptr = reused.base();
        assert!(reused_ptr == second_ptr || reused_ptr == third_ptr);
        assert_ne!(reused_ptr, first_ptr);
        drop(reused);

        assert!(cache.take_exact(len).is_none());
    }
}
