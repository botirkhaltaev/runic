use core::ptr::NonNull;

use crate::{
    config::AllocatorConfig,
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::SizeClass,
};

pub(crate) mod extent;
pub(crate) mod id;
pub(crate) mod run;
pub(crate) mod table;

pub(crate) use extent::Extent;
pub(crate) use extent::heap::{ExtentHeap, ExtentHeapError, ExtentInit};
pub(crate) use id::HeapId;
pub(crate) use run::{Run, RunError, RunHeap, RunHeapError, RunId};
pub(crate) use table::{HeapDirectory, HeapError, HeapSlot};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapMode {
    Free = 0,
    Active = 1,
    Draining = 2,
}

impl HeapMode {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Active => 1,
            Self::Draining => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Active),
            2 => Some(Self::Draining),
            _ => None,
        }
    }
}

/// Owner-local run/extent metadata. Lifecycle (mode/gen/inbox) lives on [`HeapSlot`].
pub(crate) struct Heap {
    id: HeapId,
    pub(crate) runs: RunHeap,
    pub(crate) extents: ExtentHeap,
}

// SAFETY: Heap mutation is serialized by the owning thread through TLS exclusive access,
// or by directory-locked Draining paths.
unsafe impl Send for Heap {}
// SAFETY: same invariants as `Send`; shared reads only under directory lock or owner TLS.
unsafe impl Sync for Heap {}

impl Heap {
    pub(crate) fn new(id: HeapId, capacity: u32, config: AllocatorConfig) -> Self {
        Self {
            id,
            runs: RunHeap::new(capacity),
            extents: ExtentHeap::new(capacity, config.extent()),
        }
    }

    pub(crate) fn rebind_heap_id(&mut self, id: HeapId) {
        self.id = id;
        self.runs.rebind_heap_id(id);
        self.extents.rebind_heap_id(id);
    }

    /// Obtain a run for `class`: available list or cold mmap.
    pub(crate) fn acquire_run(
        &mut self,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        self.runs.allocate(class, self.id, pages)
    }

    /// One-shot small alloc without sticky: acquire, take one block, return run.
    pub(crate) fn alloc_run(&mut self, class: SizeClass, pages: &PageMap) -> Option<NonNull<u8>> {
        let run = self.acquire_run(class, pages)?;
        // SAFETY: run was just returned by this heap's live arena.
        let ptr = unsafe { run.as_ref() }.allocate()?;
        // SAFETY: same run pointer from this heap's live arena.
        if !unsafe { run.as_ref() }.is_full() {
            let _ = self.runs.return_available(run);
        }
        Some(ptr)
    }

    pub(crate) fn alloc_extent(
        &mut self,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        self.extents.allocate(spec, self.id, pages, init)
    }

    /// Owner-local free for a resolved `PageMap` owner (run or extent).
    pub(crate) fn free(
        &mut self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        match owner {
            PageOwner::Run(run) => self.runs.free(run, ptr).map_err(HeapError::from),
            PageOwner::Extent(extent) => self
                .extents
                .free(extent, ptr, pages)
                .map_err(HeapError::from),
        }
    }

    /// Live ownership for Draining reclaim: any run with outstanding blocks, or any extent.
    pub(crate) fn has_live_allocations(&self) -> bool {
        self.runs.has_live_blocks() || self.extents.has_live_extents()
    }
}
