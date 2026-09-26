use core::alloc::{GlobalAlloc, Layout};

use runic_core::{Allocator, AllocatorConfig, ExtentConfig, HugePage, Numa, RunConfig};

/// Process-global Runic allocator.
///
/// Construct with [`RunicAlloc::new`] and optional `with_*` methods.
pub struct RunicAlloc {
    allocator: Allocator,
}

impl RunicAlloc {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allocator: Allocator::new(),
        }
    }

    /// First `init` in the process wins; later configs are ignored.
    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            allocator: Allocator::with_config(config),
        }
    }

    #[must_use]
    pub const fn with_hugepage(self, hugepage: HugePage) -> Self {
        Self {
            allocator: self.allocator.with_hugepage(hugepage),
        }
    }

    #[must_use]
    pub const fn with_numa(self, numa: Numa) -> Self {
        Self {
            allocator: self.allocator.with_numa(numa),
        }
    }

    #[must_use]
    pub const fn with_extent_config(self, extent: ExtentConfig) -> Self {
        Self {
            allocator: self.allocator.with_extent_config(extent),
        }
    }

    #[must_use]
    pub const fn with_run_config(self, run: RunConfig) -> Self {
        Self {
            allocator: self.allocator.with_run_config(run),
        }
    }
}

impl Default for RunicAlloc {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `RunicAlloc` forwards to its `Allocator`. `alloc` / `alloc_zeroed`
// receive a `GlobalAlloc` layout. `dealloc` receives a pointer this allocator
// returned for that layout. `realloc` receives null or that pointer.
unsafe impl GlobalAlloc for RunicAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is the caller's `GlobalAlloc` contract.
        unsafe { self.allocator.alloc(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by this allocator for `layout`.
        unsafe { self.allocator.dealloc(ptr, layout) };
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr` is null or was returned by this allocator for `old`.
        unsafe { self.allocator.realloc(ptr, old, new_size) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is the caller's `GlobalAlloc` contract.
        unsafe { self.allocator.alloc_zeroed(layout) }
    }
}
