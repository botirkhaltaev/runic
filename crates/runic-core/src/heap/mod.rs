use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::{
    config::AllocatorConfig, layout::LayoutSpec, memory::PageMap, size_class::SizeClassId,
};

pub(crate) mod extent;
pub(crate) mod id;
pub(crate) mod run;
pub(crate) mod table;

pub(crate) use extent::Extent;
pub(crate) use extent::heap::{ExtentHeap, ExtentHeapError, ExtentInit};
pub(crate) use id::HeapId;
pub(crate) use run::{Run, RunError, RunHeap, RunHeapError, RunId};
pub(crate) use table::{HeapError, HeapTable, Inbox, THREAD_HEAP};

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

pub(crate) struct Heap {
    mode: AtomicU8,
    id: HeapId,
    pub(crate) runs: RunHeap,
    pub(crate) extents: ExtentHeap,
    inbox: Inbox,
}

// SAFETY: Heap mutation is serialized by the owning thread through TLS exclusive access,
// or by allocator-state-serialized lifecycle paths while a heap is draining.
unsafe impl Send for Heap {}
// SAFETY: Inbox producers use atomics; mode is atomic; owner-local metadata mutation
// requires exclusive access via TLS Active or table-locked Draining.
unsafe impl Sync for Heap {}

impl Heap {
    pub(crate) fn new(id: HeapId, capacity: u32, config: AllocatorConfig) -> Self {
        Self {
            mode: AtomicU8::new(HeapMode::Active.raw()),
            id,
            runs: RunHeap::new(capacity),
            extents: ExtentHeap::new(capacity, config.extent()),
            inbox: Inbox::new(),
        }
    }

    pub(crate) const fn id(&self) -> HeapId {
        self.id
    }

    pub(crate) fn set_id(&mut self, id: HeapId) {
        self.id = id;
    }

    pub(crate) fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    pub(crate) fn is_active(&self) -> bool {
        self.mode() == HeapMode::Active
    }

    pub(crate) fn is_free(&self) -> bool {
        self.mode() == HeapMode::Free
    }

    pub(crate) fn begin_drain(&self) {
        self.mode.store(HeapMode::Draining.raw(), Ordering::Release);
    }

    pub(crate) fn reactivate(&mut self, id: HeapId) {
        self.mode.store(HeapMode::Active.raw(), Ordering::Release);
        self.set_id(id);
        self.runs.rebind_heap_id(id);
    }

    /// Mark Free when empty; caller bumps table generation.
    pub(crate) fn try_reclaim(&mut self) -> bool {
        if self.has_live_allocations() || !self.inbox.is_empty() {
            return false;
        }

        self.mode.store(HeapMode::Free.raw(), Ordering::Release);
        true
    }

    /// Snapshot of this heap's Free/Active/Draining lifecycle state.
    pub(crate) fn mode(&self) -> HeapMode {
        HeapMode::from_raw(self.mode.load(Ordering::Acquire)).unwrap_or(HeapMode::Free)
    }

    /// Obtain a run for `class`: flush inbox once if needed, then available or cold mmap.
    pub(crate) fn acquire_run(
        &mut self,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }

        self.runs.allocate(class, self.id, pages)
    }

    /// One-shot small alloc without holding a sticky run: acquire, take one block, return run.
    pub(crate) fn alloc_run(&mut self, class: SizeClassId, pages: &PageMap) -> Option<NonNull<u8>> {
        let run = self.acquire_run(class, pages)?;
        // SAFETY: run was just returned by this heap's live arena.
        let ptr = unsafe { run.as_ref() }.allocate()?;
        // SAFETY: same run pointer from this heap's live arena.
        if unsafe { run.as_ref() }.has_available_blocks() {
            let _ = self.runs.return_available(run);
        }
        Some(ptr)
    }

    pub(crate) fn allocate_extent(
        &mut self,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }

        self.extents.allocate(spec, self.id, pages, init)
    }

    /// Owner-local non-cached free: flush inbox if needed, then free.
    ///
    /// Callable via a TLS-bound `Heap` without taking the table mutex. Does not wrap the
    /// cached-run hit (`Run::free_local`); that path stays on `ThreadHeap::free` for minimal work.
    pub(crate) fn free_run_owner(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.inbox.is_empty() {
            self.flush(pages)?;
        }
        self.runs.free(run, ptr).map_err(HeapError::from)
    }

    /// Owner-local extent free: flush inbox if needed, then free.
    pub(crate) fn free_extent_owner(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.inbox.is_empty() {
            self.flush(pages)?;
        }
        self.extents
            .free(extent, ptr, pages)
            .map_err(HeapError::from)
    }

    pub(crate) fn flush(&mut self, pages: &PageMap) -> Result<(), HeapError> {
        while let Some(list) = self.inbox.drain() {
            for ptr in list {
                match pages.get(ptr) {
                    Some(crate::memory::PageOwner::Run(run)) => {
                        self.runs.complete_remote_free(run, ptr)?;
                    }
                    Some(crate::memory::PageOwner::Extent(extent)) => {
                        self.extents.complete_remote_free(extent, ptr, pages)?;
                    }
                    None => return Err(HeapError::InvalidPointer),
                }
            }
        }

        Ok(())
    }

    /// Live ownership for Draining reclaim: any run with outstanding blocks, or any extent.
    pub(crate) fn has_live_allocations(&self) -> bool {
        self.runs.has_live_blocks() || self.extents.has_live_extents()
    }
}
