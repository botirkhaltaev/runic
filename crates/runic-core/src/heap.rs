use core::ptr::NonNull;

use crate::{
    allocation::Allocation,
    config::AllocatorConfig,
    extent::{Extent, ExtentHeap, ExtentHeapError},
    layout::LayoutSpec,
    memory::PageMap,
    ownership::HeapOwner,
    run::{Run, RunHeap, RunHeapError},
    size_class::SizeClasses,
};

pub(crate) struct SharedHeap {
    runs: RunHeap,
    extents: ExtentHeap,
}

impl SharedHeap {
    pub(crate) const DEFAULT_METADATA_CAPACITY: u32 = 65_536;

    pub(crate) const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            runs: RunHeap::new(Self::DEFAULT_METADATA_CAPACITY, config.run()),
            extents: ExtentHeap::new(Self::DEFAULT_METADATA_CAPACITY, config.extent()),
        }
    }

    pub(crate) fn allocate(&mut self, spec: LayoutSpec, pages: &mut PageMap) -> Option<Allocation> {
        match SizeClasses::id_for(spec) {
            Some(class) => self.runs.allocate(class, HeapOwner::Shared, pages),
            None => self.extents.allocate(spec, HeapOwner::Shared, pages),
        }
    }

    pub(crate) fn free_run(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), RunHeapError> {
        self.runs.free(run, ptr, pages)
    }

    pub(crate) fn free_extent(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), ExtentHeapError> {
        self.extents.free(extent, ptr, pages)
    }
}
