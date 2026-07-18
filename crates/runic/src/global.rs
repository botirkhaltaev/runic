use core::alloc::{GlobalAlloc, Layout};

use runic_core::{Allocator, AllocatorConfig, Budget, ExtentPolicy, ExtentReuse};

pub struct RunicAlloc {
    allocator: Allocator,
}

impl RunicAlloc {
    #[must_use]
    pub const fn new() -> Self {
        Self::with_config(AllocatorConfig::new())
    }

    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            allocator: Allocator::with_config(config),
        }
    }

    #[must_use]
    pub const fn builder() -> RunicAllocBuilder {
        RunicAllocBuilder::new()
    }
}

impl Default for RunicAlloc {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RunicAllocBuilder {
    config: AllocatorConfig,
}

impl RunicAllocBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: AllocatorConfig::new(),
        }
    }

    #[must_use]
    pub const fn extent(self) -> ExtentBuilder {
        ExtentBuilder { builder: self }
    }

    #[must_use]
    pub const fn build(self) -> RunicAlloc {
        RunicAlloc::with_config(self.config)
    }
}

impl Default for RunicAllocBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ExtentBuilder {
    builder: RunicAllocBuilder,
}

impl ExtentBuilder {
    #[must_use]
    pub const fn policy(mut self, policy: ExtentPolicy) -> Self {
        self.builder.config = self.builder.config.with_extent_policy(policy);
        self
    }

    #[must_use]
    pub const fn reuse(mut self, reuse: ExtentReuse) -> Self {
        self.builder.config = self.builder.config.with_extent_reuse(reuse);
        self
    }

    #[must_use]
    pub const fn budget(mut self, budget: Budget) -> Self {
        self.builder.config = self.builder.config.with_extent_budget(budget);
        self
    }

    #[must_use]
    pub const fn done(self) -> RunicAllocBuilder {
        self.builder
    }
}

unsafe impl GlobalAlloc for RunicAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.allocator.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.allocator.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        unsafe { self.allocator.realloc(ptr, old, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { self.allocator.alloc_zeroed(layout) }
    }
}
