use std::{
    alloc::{GlobalAlloc, Layout, System},
    ptr::NonNull,
};

use mimalloc::MiMalloc;
use runic::RunicAlloc;
use snmalloc_rs::SnMalloc;
use tikv_jemallocator::Jemalloc;

static RUNIC: RunicAlloc = RunicAlloc::new();
static SYSTEM: System = System;
static MIMALLOC: MiMalloc = MiMalloc;
static JEMALLOC: Jemalloc = Jemalloc;
static SNMALLOC: SnMalloc = SnMalloc;

#[derive(Clone, Copy)]
pub struct AllocatorTarget {
    name: &'static str,
    allocator: &'static (dyn GlobalAlloc + Sync),
}

unsafe impl Send for AllocatorTarget {}
unsafe impl Sync for AllocatorTarget {}

impl AllocatorTarget {
    #[must_use]
    pub const fn new(name: &'static str, allocator: &'static (dyn GlobalAlloc + Sync)) -> Self {
        Self { name, allocator }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Allocates memory with this target allocator.
    ///
    /// # Panics
    ///
    /// Panics if the target allocator returns null.
    #[must_use]
    pub fn alloc(self, layout: Layout) -> NonNull<u8> {
        let ptr = unsafe { self.allocator.alloc(layout) };
        NonNull::new(ptr).unwrap_or_else(|| panic!("{} returned null for {layout:?}", self.name))
    }

    /// Allocates zero-initialized memory with this target allocator.
    ///
    /// # Panics
    ///
    /// Panics if the target allocator returns null.
    #[must_use]
    pub fn alloc_zeroed(self, layout: Layout) -> NonNull<u8> {
        let ptr = unsafe { self.allocator.alloc_zeroed(layout) };
        NonNull::new(ptr)
            .unwrap_or_else(|| panic!("{} returned null for zeroed {layout:?}", self.name))
    }

    pub fn dealloc(self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { self.allocator.dealloc(ptr.as_ptr(), layout) };
    }

    /// Reallocates memory with this target allocator.
    ///
    /// # Panics
    ///
    /// Panics if the target allocator returns null.
    #[must_use]
    pub fn realloc(self, ptr: NonNull<u8>, old: Layout, new_size: usize) -> NonNull<u8> {
        let ptr = unsafe { self.allocator.realloc(ptr.as_ptr(), old, new_size) };
        NonNull::new(ptr).unwrap_or_else(|| {
            panic!(
                "{} returned null for realloc from {old:?} to {new_size}",
                self.name
            )
        })
    }
}

pub const TARGETS: &[AllocatorTarget] = &[
    AllocatorTarget::new("runic", &RUNIC),
    AllocatorTarget::new("system", &SYSTEM),
    AllocatorTarget::new("mimalloc", &MIMALLOC),
    AllocatorTarget::new("jemalloc", &JEMALLOC),
    AllocatorTarget::new("snmalloc", &SNMALLOC),
];

pub const RUNIC_TARGETS: &[AllocatorTarget] = &[AllocatorTarget::new("runic", &RUNIC)];

#[must_use]
pub fn target_by_name(name: &str) -> Option<AllocatorTarget> {
    TARGETS.iter().copied().find(|target| target.name() == name)
}
