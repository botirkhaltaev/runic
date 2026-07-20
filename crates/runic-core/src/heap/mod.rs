use core::{
    cell::Cell,
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
pub(crate) use run::{RUN_SIZE, Run, RunError, RunHeap, RunHeapError, RunId};
pub(crate) use table::{HeapError, HeapTable, Inbox, RemoteList, THREAD_HEAP};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeapMode {
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
    alloc_count: Cell<u32>,
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
            alloc_count: Cell::new(0),
            inbox: Inbox::new(),
        }
    }

    pub(crate) const fn id(&self) -> HeapId {
        self.id
    }

    pub(crate) fn set_id(&mut self, id: HeapId) {
        self.id = id;
    }

    pub(crate) fn rebind_available_runs(&mut self, heap_id: HeapId) {
        self.runs.rebind_available(heap_id);
    }

    pub(crate) fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    pub(crate) fn is_active(&self) -> bool {
        self.mode() == HeapMode::Active
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.mode() == HeapMode::Draining
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
        self.rebind_available_runs(id);
    }

    /// Mark Free when empty; caller bumps table generation.
    pub(crate) fn try_reclaim(&mut self) -> bool {
        if self.has_live_allocations() || !self.inbox.is_empty() {
            return false;
        }

        self.mode.store(HeapMode::Free.raw(), Ordering::Release);
        true
    }

    fn mode(&self) -> HeapMode {
        HeapMode::from_raw(self.mode.load(Ordering::Acquire)).unwrap_or(HeapMode::Free)
    }

    pub(crate) fn allocate_run(
        &mut self,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }

        let mut run = self.runs.allocate(class, self.id, pages)?;
        // SAFETY: RunHeap returns pointers to live runs from this heap's arena.
        let ptr = unsafe { run.as_mut() }.allocate()?;
        self.retain_allocation();
        // SAFETY: RunHeap returns pointers to live runs from this heap's arena.
        if unsafe { run.as_ref() }.has_available_blocks() {
            let _ = self.runs.return_available(run);
        }
        Some(ptr)
    }

    pub(crate) fn take_or_allocate_run(
        &mut self,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }

        // `RunHeap::allocate` already tries available then cold allocate_run.
        self.runs.allocate(class, self.id, pages)
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

        let ptr = self.extents.allocate(spec, self.id, pages, init)?;
        self.retain_allocation();
        Some(ptr)
    }

    pub(crate) fn free_run(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), RunHeapError> {
        self.runs.free(run, ptr)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn complete_remote_run(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), RunHeapError> {
        self.runs.complete_remote_free(run, ptr)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn free_extent(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        self.extents.free(extent, ptr, pages)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn complete_remote_extent(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        self.extents.complete_remote_free(extent, ptr, pages)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn flush(&mut self, pages: &PageMap) -> Result<(), HeapError> {
        while let Some(list) = self.inbox.drain() {
            for ptr in list {
                match pages.get(ptr) {
                    Some(crate::memory::PageOwner::Run(run)) => {
                        self.complete_remote_run(run, ptr)?;
                    }
                    Some(crate::memory::PageOwner::Extent(extent)) => {
                        self.complete_remote_extent(extent, ptr, pages)?;
                    }
                    None => return Err(HeapError::InvalidPointer),
                }
            }
        }

        Ok(())
    }

    pub(crate) fn has_live_allocations(&self) -> bool {
        self.alloc_count.get() != 0
    }

    pub(crate) fn retain_allocation(&self) {
        let Some(live) = self.alloc_count.get().checked_add(1) else {
            Self::abort();
        };
        self.alloc_count.set(live);
    }

    pub(crate) fn release_allocation(&self) {
        let Some(live) = self.alloc_count.get().checked_sub(1) else {
            Self::abort();
        };
        self.alloc_count.set(live);
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }
}
