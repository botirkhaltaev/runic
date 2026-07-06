use core::mem::MaybeUninit;

use crate::memory::Mapping;

pub(crate) struct MappingCache {
    slots: [ExtentMappingSlot; Self::SLOTS],
    retained_bytes: usize,
}

impl MappingCache {
    const SLOTS: usize = 32;
    const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { ExtentMappingSlot::empty() }; Self::SLOTS],
            retained_bytes: 0,
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
        self.retained_bytes
            .checked_add(len)
            .is_some_and(|retained_bytes| retained_bytes <= Self::MAX_RETAINED_BYTES)
            && self.slots.iter().any(ExtentMappingSlot::is_empty)
    }

    pub(crate) fn insert(&mut self, mapping: Mapping) -> Result<(), Mapping> {
        let len = mapping.range().len();
        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(mapping);
        };

        if retained_bytes > Self::MAX_RETAINED_BYTES {
            return Err(mapping);
        }

        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_empty()) else {
            return Err(mapping);
        };

        slot.insert(mapping);
        self.retained_bytes = retained_bytes;

        Ok(())
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
}

impl ExtentMappingSlot {
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
}
