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

    pub(crate) fn acquire(
        &mut self,
        extents: &Arena<Extent>,
        len: usize,
    ) -> Result<Option<ExtentId>, crate::heap::HeapError> {
        let mut prev: Option<ExtentId> = None;
        let mut current = self.head;
        while let Some(id) = current {
            let Some(extent) = extents.get(id.index()) else {
                return Err(crate::heap::HeapError::MissingExtent);
            };
            if extent.mapping().len().get() == len {
                let next = extent.next();
                match prev {
                    Some(previous) => {
                        let Some(previous) = extents.get(previous.index()) else {
                            return Err(crate::heap::HeapError::MissingExtent);
                        };
                        previous.set_next(next);
                    }
                    None => self.head = next,
                }
                extent.set_next(None);
                debug_assert!(self.count >= 1);
                debug_assert!(self.retained_bytes >= len);
                self.count -= 1;
                self.retained_bytes -= len;
                return Ok(Some(id));
            }
            prev = Some(id);
            current = extent.next();
        }
        Ok(None)
    }

    fn will_retain(&self, len: usize) -> bool {
        if !self.config.policy().retains() {
            return false;
        }

        let budget = self.config.budget();
        self.count < budget.slots()
            && budget.bytes() >= len
            && self.retained_bytes <= budget.bytes() - len
    }

    pub(crate) fn insert(&mut self, extent: &Extent) -> bool {
        let len = extent.mapping().len().get();
        if !self.will_retain(len) {
            return false;
        }

        extent.set_next(self.head);
        self.head = Some(extent.id());
        self.count += 1;
        self.retained_bytes += len;
        if self.config.policy() == ExtentPolicy::Discard {
            extent.discard();
        }
        true
    }
}
