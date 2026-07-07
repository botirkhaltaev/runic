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
    pub const fn with_extent_reuse(mut self, reuse: ExtentReuse) -> Self {
        self.extent = self.extent.with_reuse(reuse);
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

    #[must_use]
    pub const fn with_run_budget(mut self, budget: Budget) -> Self {
        self.run = self.run.with_budget(budget);
        self
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::new()
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunConfig {
    policy: RunPolicy,
    budget: Budget,
}

impl RunConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: RunPolicy::Keep,
            budget: Budget::new(0, 0),
        }
    }

    #[must_use]
    pub const fn policy(self) -> RunPolicy {
        self.policy
    }

    #[must_use]
    pub const fn budget(self) -> Budget {
        self.budget
    }

    pub(crate) const fn retains_empty_runs(self) -> bool {
        matches!(
            self.policy,
            RunPolicy::RetainFifo | RunPolicy::RetainPerClass
        )
    }

    #[must_use]
    pub const fn with_policy(mut self, policy: RunPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub const fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        Self::new()
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentPolicy {
    Drop,
    Keep,
    Fifo,
    Lifo,
    Largest,
    Smallest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentReuse {
    Exact,
    BestFit,
    SizeClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunPolicy {
    Keep,
    DropEmpty,
    RetainFifo,
    RetainPerClass,
}
