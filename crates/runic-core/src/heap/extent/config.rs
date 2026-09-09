use crate::config::Budget;

/// Dedicated extent mapping cache configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentConfig {
    policy: ExtentPolicy,
    budget: Budget,
}

impl ExtentConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: ExtentPolicy::Keep,
            budget: Budget::new(64, 64 * 1024 * 1024),
        }
    }

    #[must_use]
    pub const fn policy(self) -> ExtentPolicy {
        self.policy
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

/// Retention policy for freed dedicated extent mappings.
///
/// Allocation-side lookup always reuses a retained mapping with exactly the
/// requested length; there is no size-bucket or best-fit reuse strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentPolicy {
    /// Retain a freed mapping only while both slot and byte budget have free
    /// capacity; otherwise the mapping is unmapped. Keep never evicts a
    /// retained mapping to admit another. Reused pages stay dirty;
    /// `alloc_zeroed` memsets on cache hit.
    Keep,
    /// Like [`Self::Keep`], then `madvise(MADV_DONTNEED)` on the mapping.
    /// Zeroed reuse skips memset when discard succeeded (kernel zeros on fault).
    Discard,
    /// Do not retain freed extent mappings; unmap immediately. Useful for tests
    /// and benchmarks that compare against unretained large-allocation churn.
    Unmap,
}

impl ExtentPolicy {
    /// `Unmap` does not retain; every other policy does.
    pub(crate) const fn retains(self) -> bool {
        !matches!(self, Self::Unmap)
    }
}
