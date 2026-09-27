use super::super::list::LinkedList;
use super::{
    Extent,
    config::{ExtentConfig, ExtentPolicy},
};

/// Retained published extents, linked through [`list::Link`](super::super::list::Link).
///
/// The cache owns reuse policy. An extent is on this list or the unmapped-slot
/// list, never both.
pub(crate) struct ExtentCache {
    extents: LinkedList<'static, Extent>,
    count: usize,
    retained_bytes: usize,
    config: ExtentConfig,
}

impl ExtentCache {
    pub(crate) const fn new(config: ExtentConfig) -> Self {
        Self {
            extents: LinkedList::new(),
            count: 0,
            retained_bytes: 0,
            config,
        }
    }

    pub(crate) fn take(&mut self, len: usize) -> Option<&'static Extent> {
        let mut cursor = self.extents.cursor_front_mut();
        while let Some(extent) = cursor.current() {
            if extent.mapping().len().get() == len {
                cursor.remove_current();
                debug_assert!(self.count >= 1);
                debug_assert!(self.retained_bytes >= len);
                self.count -= 1;
                self.retained_bytes -= len;
                return Some(extent);
            }
            cursor.move_next();
        }
        None
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

    pub(crate) fn insert(&mut self, extent: &'static Extent) -> bool {
        let len = extent.mapping().len().get();
        if !self.will_retain(len) {
            return false;
        }

        self.extents.push_front(extent);
        self.count += 1;
        self.retained_bytes += len;
        if self.config.policy() == ExtentPolicy::Discard {
            extent.discard();
        }
        true
    }
}
