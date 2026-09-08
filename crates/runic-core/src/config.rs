use crate::heap::extent::config::{ExtentConfig, ExtentPolicy};
use crate::heap::run::config::{RunConfig, RunPolicy};

/// Immutable allocator configuration for tunable allocator behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorConfig {
    extent: ExtentConfig,
    run: RunConfig,
}

impl AllocatorConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            extent: ExtentConfig::new(),
            run: RunConfig::new(),
        }
    }

    #[must_use]
    pub const fn extent(self) -> ExtentConfig {
        self.extent
    }

    #[must_use]
    pub const fn run(self) -> RunConfig {
        self.run
    }

    #[must_use]
    pub const fn with_extent_policy(mut self, policy: ExtentPolicy) -> Self {
        self.extent = self.extent.with_policy(policy);
        self
    }

    #[must_use]
    pub const fn with_extent_budget(mut self, budget: Budget) -> Self {
        self.extent = self.extent.with_budget(budget);
        self
    }

    #[must_use]
    pub const fn with_run_policy(mut self, policy: RunPolicy) -> Self {
        self.run = self.run.with_policy(policy);
        self
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Slot and byte limits for allocator mapping caches.
///
/// Slot and byte limits are enforced exactly. There is no internal clamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    slots: usize,
    bytes: usize,
}

impl Budget {
    #[must_use]
    pub const fn new(slots: usize, bytes: usize) -> Self {
        Self { slots, bytes }
    }

    #[must_use]
    pub const fn slots(self) -> usize {
        self.slots
    }

    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}
