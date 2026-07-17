/// Immutable allocator configuration for tunable allocator behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorConfig {
    extent: ExtentConfig,
}

impl AllocatorConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            extent: ExtentConfig::new(),
        }
    }

    #[must_use]
    pub const fn extent(self) -> ExtentConfig {
        self.extent
    }

    #[must_use]
    pub const fn with_extent_policy(mut self, policy: ExtentPolicy) -> Self {
        self.extent = self.extent.with_policy(policy);
        self
    }

    #[must_use]
    pub const fn with_extent_reuse(mut self, reuse: ExtentReuse) -> Self {
        self.extent = self.extent.with_reuse(reuse);
        self
    }

    #[must_use]
    pub const fn with_extent_budget(mut self, budget: Budget) -> Self {
        self.extent = self.extent.with_budget(budget);
        self
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Dedicated extent mapping cache configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentConfig {
    policy: ExtentPolicy,
    reuse: ExtentReuse,
    budget: Budget,
}

impl ExtentConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: ExtentPolicy::Keep,
            reuse: ExtentReuse::Exact,
            budget: Budget::new(32, 16 * 1024 * 1024),
        }
    }

    #[must_use]
    pub const fn policy(self) -> ExtentPolicy {
        self.policy
    }

    #[must_use]
    pub const fn reuse(self) -> ExtentReuse {
        self.reuse
    }

    #[must_use]
    pub const fn budget(self) -> Budget {
        self.budget
    }

    #[must_use]
    pub const fn with_policy(mut self, policy: ExtentPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub const fn with_reuse(mut self, reuse: ExtentReuse) -> Self {
        self.reuse = reuse;
        self
    }

    #[must_use]
    pub const fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }
}

impl Default for ExtentConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Slot and byte limits for allocator mapping caches.
///
/// Cache implementations use fixed internal storage and clamp active slots to
/// their internal maximum. The byte limit is still enforced exactly.
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

/// Retention and eviction policy for freed dedicated extent mappings.
///
/// This controls which mappings remain cached after free and which cached
/// mapping is evicted when space is needed. Allocation-side lookup order is
/// controlled separately by [`ExtentReuse`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentPolicy {
    /// Do not retain freed extent mappings.
    Drop,
    /// Retain only while both slot and byte budget have free capacity.
    Keep,
    /// Evict the oldest retained mapping when capacity is needed.
    Fifo,
    /// Evict the newest retained mapping when capacity is needed.
    Lifo,
    /// Evict the largest retained mapping when capacity is needed.
    Largest,
    /// Evict the smallest retained mapping when capacity is needed.
    Smallest,
}

/// Allocation-side lookup strategy for cached extent mappings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentReuse {
    /// Reuse only mappings with exactly the requested mapping length.
    Exact,
    /// Reuse the smallest retained mapping that can satisfy the request.
    BestFit,
    /// Reuse a sufficiently large mapping from the request's power-of-two size bucket.
    SizeClass,
}
