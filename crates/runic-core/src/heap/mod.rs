use core::{cell::Cell, ptr::NonNull};

use spin::Mutex;

use crate::{
    config::AllocatorConfig, layout::LayoutSpec, memory::PageMap, size_class::SizeClassId,
};

pub(crate) mod extent;
pub(crate) mod id;
pub(crate) mod run;
pub(crate) mod table;

pub(crate) use extent::{Extent, ExtentArena, ExtentHeap, ExtentHeapError, ExtentId};
pub(crate) use id::HeapId;
pub(crate) use run::{RUN_SIZE, Run, RunArena, RunError, RunHeap, RunHeapError, RunId, RunOwner};
pub(crate) use table::{HeapError, HeapHandle, HeapTable, THREAD_HEAP};

pub(crate) struct Heap {
    id: HeapId,
    runs: Mutex<RunHeap>,
    extents: Mutex<ExtentHeap>,
    live: Cell<u32>,
}

// SAFETY: Heap uses interior synchronization for mutable allocation metadata. Moving ownership to
// another thread does not permit unsynchronized metadata mutation.
unsafe impl Send for Heap {}
// SAFETY: Heap uses interior synchronization for shared run/extent metadata. The live count is
// mutated only by the owner thread while active, or by allocator-state-serialized lifecycle paths
// after abandonment; remote frees do not mutate it until an owner/lifecycle drain completes them.
unsafe impl Sync for Heap {}

impl Heap {
    pub(crate) const DEFAULT_METADATA_CAPACITY: u32 = 65_536;

    pub(crate) const fn root(config: AllocatorConfig) -> Self {
        Self::new(HeapId::ROOT, Self::DEFAULT_METADATA_CAPACITY, config)
    }

    pub(crate) const fn new(id: HeapId, capacity: u32, config: AllocatorConfig) -> Self {
        Self {
            id,
            runs: Mutex::new(RunHeap::new(capacity)),
            extents: Mutex::new(ExtentHeap::new(capacity, config.extent())),
            live: Cell::new(0),
        }
    }

    pub(crate) fn allocate_run(&self, class: SizeClassId, pages: &PageMap) -> Option<NonNull<u8>> {
        let mut runs = self.runs.lock();
        let mut run = runs.allocate(class, self.id, pages)?;
        // SAFETY: RunHeap returns pointers to live runs from this heap's arena.
        let ptr = unsafe { run.as_mut() }.allocate()?;
        // SAFETY: RunHeap returns pointers to live runs from this heap's arena.
        if unsafe { run.as_ref() }.has_available_blocks() {
            runs.return_available(run).ok()?;
        }
        self.retain_allocation();
        Some(ptr)
    }

    pub(crate) fn take_available_run(&self, class: SizeClassId) -> Option<NonNull<Run>> {
        self.runs.lock().take_available(class)
    }

    pub(crate) fn return_run(&self, run: NonNull<Run>) -> Result<(), RunHeapError> {
        self.runs.lock().return_available(run)
    }

    pub(crate) fn allocate_cached_run(&self, mut run: NonNull<Run>) -> Option<NonNull<u8>> {
        // SAFETY: cached run pointers are published from this heap's live RunArena and retained by
        // the owning TLS heap entry while the heap slot remains installed.
        let ptr = unsafe { run.as_mut() }.allocate()?;
        self.retain_allocation();
        Some(ptr)
    }

    pub(crate) fn free_cached_run(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), RunHeapError> {
        // SAFETY: cached run pointers are published from this heap's live RunArena and retained by
        // the owning TLS heap entry while the heap slot remains installed.
        unsafe { run.as_ref() }
            .free_local(ptr)
            .map_err(RunHeapError::from)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn allocate_extent(&self, spec: LayoutSpec, pages: &PageMap) -> Option<NonNull<u8>> {
        let ptr = self.extents.lock().allocate(spec, self.id, pages)?;
        self.retain_allocation();
        Some(ptr)
    }

    pub(crate) fn allocate_zeroed_extent(
        &self,
        spec: LayoutSpec,
        requested_size: usize,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let ptr = self
            .extents
            .lock()
            .allocate_zeroed(spec, requested_size, self.id, pages)?;
        self.retain_allocation();
        Some(ptr)
    }

    pub(crate) fn free_run(&self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), RunHeapError> {
        self.runs.lock().free(run, ptr)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn mark_remote_run(run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), RunHeapError> {
        RunHeap::mark_remote_pending(run, ptr)
    }

    pub(crate) fn complete_remote_run(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), RunHeapError> {
        self.runs.lock().complete_remote_free(run, ptr)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn free_extent(
        &self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), ExtentHeapError> {
        self.extents.lock().free(extent, ptr, pages)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn has_live_allocations(&self) -> bool {
        self.live.get() != 0
    }

    fn retain_allocation(&self) {
        let Some(live) = self.live.get().checked_add(1) else {
            Self::abort();
        };
        self.live.set(live);
    }

    fn release_allocation(&self) {
        let Some(live) = self.live.get().checked_sub(1) else {
            Self::abort();
        };
        self.live.set(live);
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }
}
