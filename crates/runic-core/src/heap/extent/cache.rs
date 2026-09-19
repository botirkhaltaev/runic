use crate::{
    arena::Arena,
    heap::{
        Extent,
        extent::{
            ExtentId,
            config::{ExtentConfig, ExtentPolicy},
        },
    },
};

/// Intrusive index list of retained published extents.
///
/// Links are [`ExtentId`] values into the owning [`super::heap::ExtentHeap`]
/// arena. The cache owns reuse policy; the arena owns metadata storage.
pub(crate) struct ExtentCache {
    head: Option<ExtentId>,
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

    pub(crate) fn acquire(&mut self, extents: &Arena<Extent>, len: usize) -> Option<ExtentId> {
        let mut prev: Option<ExtentId> = None;
        let mut current = self.head;
        while let Some(id) = current {
            let extent = extents.get(id.index())?;
            if extent.mapping().len().get() == len {
                let next = extent.next();
                if let Some(prev) = prev {
                    extents.get(prev.index())?.set_next(next);
                } else {
                    self.head = next;
                }
                extent.set_next(None);
                debug_assert!(self.count >= 1);
                debug_assert!(self.retained_bytes >= len);
                self.count -= 1;
                self.retained_bytes -= len;
                return Some(id);
            }
            prev = Some(id);
            current = extent.next();
        }
        None
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

    pub(crate) fn insert(&mut self, extents: &Arena<Extent>, id: ExtentId) -> Result<(), ExtentId> {
        let Some(extent) = extents.get(id.index()) else {
            return Err(id);
        };
        let len = extent.mapping().len().get();
        if !self.will_retain(len) {
            return Err(id);
        }
        let Some(retained_bytes) = self.retained_bytes.checked_add(len) else {
            return Err(id);
        };

        extent.set_next(self.head);
        self.head = Some(id);
        self.count += 1;
        self.retained_bytes = retained_bytes;
        if self.config.policy() == ExtentPolicy::Discard {
            extent.discard();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use std::sync::LazyLock;

    use crate::{
        config::{AllocatorConfig, Budget},
        heap::{Heap, HeapId},
        layout::LayoutSpec,
        memory::OsMemory,
    };

    use super::*;

    static OWNER: LazyLock<Heap> = LazyLock::new(|| {
        Heap::new(
            HeapId::new(0, core::num::NonZeroU32::MIN).unwrap(),
            AllocatorConfig::new(),
        )
    });

    struct Extents(Arena<Extent>);

    impl Extents {
        fn new() -> Self {
            Self(Arena::new())
        }

        fn insert(&mut self, mapping_len: usize) -> ExtentId {
            let index = self.0.vacant().unwrap();
            let id = ExtentId::from_index(index).unwrap();
            let spec = LayoutSpec::from_layout(Layout::from_size_align(mapping_len, 8).unwrap());
            let mapping = OsMemory::map(mapping_len).unwrap();
            let extent = Extent::new(id, &OWNER, mapping, spec).unwrap();
            assert_eq!(extent.free(extent.ptr()), Ok(()));
            self.0.insert(index, extent).unwrap();
            id
        }
    }

    #[test]
    fn reuses_exact_length_and_unlinks_middle() {
        let mut cache = ExtentCache::new(ExtentConfig::new());
        let mut extents = Extents::new();
        let first = extents.insert(64 * 1024);
        let middle = extents.insert(128 * 1024);
        let last = extents.insert(256 * 1024);

        assert!(cache.insert(&extents.0, first).is_ok());
        assert!(cache.insert(&extents.0, middle).is_ok());
        assert!(cache.insert(&extents.0, last).is_ok());
        assert_eq!(cache.acquire(&extents.0, 128 * 1024), Some(middle));
        assert_eq!(cache.acquire(&extents.0, 64 * 1024), Some(first));
        assert_eq!(cache.acquire(&extents.0, 256 * 1024), Some(last));
        assert!(cache.acquire(&extents.0, 128 * 1024).is_none());
    }

    #[test]
    fn enforces_slot_and_byte_budgets() {
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Keep)
                .with_budget(Budget::new(2, 8192)),
        );
        let mut extents = Extents::new();
        let first = extents.insert(4096);
        let second = extents.insert(4096);
        let third = extents.insert(4096);

        assert!(cache.insert(&extents.0, first).is_ok());
        assert!(cache.insert(&extents.0, second).is_ok());
        assert!(cache.insert(&extents.0, third).is_err());
    }

    #[test]
    fn discard_retains_and_unmap_rejects() {
        let mut extents = Extents::new();
        let discard = extents.insert(4096);
        let mut cache = ExtentCache::new(
            ExtentConfig::new()
                .with_policy(ExtentPolicy::Discard)
                .with_budget(Budget::new(1, 4096)),
        );
        assert!(cache.insert(&extents.0, discard).is_ok());
        assert_eq!(cache.acquire(&extents.0, 4096), Some(discard));

        let unmap = extents.insert(4096);
        let mut cache = ExtentCache::new(ExtentConfig::new().with_policy(ExtentPolicy::Unmap));
        assert!(cache.insert(&extents.0, unmap).is_err());
    }
}
