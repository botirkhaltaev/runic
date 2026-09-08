/// Retention of resident pages on an empty run.
///
/// Heap maps stay mapped. `Discard` only drops payload pages via `madvise`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunPolicy {
    /// Leave empty-run payload pages resident for reuse.
    Keep,
    /// Discard empty-run payload pages. The next checkout rebuilds via `extend`.
    Discard,
}

/// Empty-run resident-page policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunConfig {
    policy: RunPolicy,
}

impl RunConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: RunPolicy::Keep,
        }
    }

    #[must_use]
    pub const fn policy(self) -> RunPolicy {
        self.policy
    }

    #[must_use]
    pub const fn with_policy(mut self, policy: RunPolicy) -> Self {
        self.policy = policy;
        self
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        Self::new()
    }
}
