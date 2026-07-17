use core::{cell::Cell, ptr::NonNull};

use crate::{
    allocator::{AllocatorCore, AllocatorState},
    heap::{HeapId, Run},
    memory::PageMap,
    size_class::{SizeClassId, SizeClasses},
};

use super::slot::{HeapError, HeapHandle, HeapSlot};

pub(crate) struct ThreadHeap {
    core: Cell<*mut AllocatorCore>,
    heap: Cell<Option<HeapId>>,
    slot: Cell<*mut HeapSlot>,
    runs: [Cell<*mut Run>; SizeClasses::COUNT],
}

impl Drop for ThreadHeap {
    fn drop(&mut self) {
        self.release_current();
    }
}

impl ThreadHeap {
    const fn new() -> Self {
        Self {
            core: Cell::new(core::ptr::null_mut()),
            heap: Cell::new(None),
            slot: Cell::new(core::ptr::null_mut()),
            runs: [const { Cell::new(core::ptr::null_mut()) }; SizeClasses::COUNT],
        }
    }

    pub(crate) fn allocate_run(
        &self,
        core: NonNull<AllocatorCore>,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.matches(core) {
            return None;
        }

        self.allocate_run_current(class, pages)
    }

    pub(crate) fn free_run(
        &self,
        core: NonNull<AllocatorCore>,
        heap: HeapId,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Option<Result<(), HeapError>> {
        if !self.matches(core) || self.heap.get()? != heap {
            return None;
        }

        Some(self.free_run_current(run, ptr))
    }

    pub(crate) fn heap_id(&self, core: NonNull<AllocatorCore>) -> Option<HeapId> {
        self.matches(core).then_some(())?;
        self.heap.get()
    }

    pub(crate) fn release_if_different(&self, core: NonNull<AllocatorCore>) {
        if !self.is_empty() && !self.matches(core) {
            self.release_current();
        }
    }

    pub(crate) fn get_or_acquire(
        &self,
        core: NonNull<AllocatorCore>,
        state: &mut AllocatorState,
    ) -> Option<HeapId> {
        if self.matches(core) {
            return self.heap.get();
        }

        debug_assert!(self.is_empty());
        if !AllocatorCore::retain(core) {
            return None;
        }

        let Some(handle) = state.acquire_heap() else {
            AllocatorCore::release(core);
            return None;
        };
        let heap = handle.id();
        self.install(core, handle);

        Some(heap)
    }

    fn install(&self, core: NonNull<AllocatorCore>, handle: HeapHandle) {
        self.slot.set(handle.slot_ptr().as_ptr());
        self.heap.set(Some(handle.id()));
        self.core.set(core.as_ptr());
    }

    fn matches(&self, core: NonNull<AllocatorCore>) -> bool {
        self.core.get() == core.as_ptr()
    }

    fn is_empty(&self) -> bool {
        self.core.get().is_null()
    }

    fn slot(&self) -> Option<NonNull<HeapSlot>> {
        NonNull::new(self.slot.get())
    }

    fn allocate_run_current(&self, class: SizeClassId, pages: &PageMap) -> Option<NonNull<u8>> {
        let slot = self.installed_slot();

        if let Some(allocation) = self.allocate_cached_run(class, slot) {
            return Some(allocation);
        }

        // SAFETY: slot is installed only while this TLS heap retains the allocator core.
        let run = unsafe { slot.as_ref() }.take_run(class, pages)?;
        self.cache_run(class, run);

        self.allocate_cached_run(class, slot)
    }

    fn allocate_cached_run(
        &self,
        class: SizeClassId,
        slot: NonNull<HeapSlot>,
    ) -> Option<NonNull<u8>> {
        let run = self.cached_run(class)?;

        // SAFETY: slot is installed only while this TLS heap retains the allocator core.
        let allocation = unsafe { slot.as_ref() }.allocate_cached_run(run);
        if allocation.is_none() {
            self.clear_run(class);
        }

        allocation
    }

    fn free_run_current(&self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), HeapError> {
        let slot = self.installed_slot();
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        let class = unsafe { run.as_ref() }.class();

        if self.cached_run(class) == Some(run) {
            // SAFETY: slot is installed only while this TLS heap retains the allocator core.
            return unsafe { slot.as_ref() }.free_cached_run(run, ptr);
        }

        // SAFETY: slot is installed only while this TLS heap retains the allocator core.
        unsafe { slot.as_ref() }.free_run(run, ptr)
    }

    fn cached_run(&self, class: SizeClassId) -> Option<NonNull<Run>> {
        NonNull::new(self.run_cell(class).get())
    }

    fn cache_run(&self, class: SizeClassId, run: NonNull<Run>) {
        self.run_cell(class).set(run.as_ptr());
    }

    fn clear_run(&self, class: SizeClassId) {
        self.run_cell(class).set(core::ptr::null_mut());
    }

    fn run_cell(&self, class: SizeClassId) -> &Cell<*mut Run> {
        debug_assert!(class.index() < self.runs.len());
        // SAFETY: SizeClassId values are created only by SizeClasses for indexes in this array.
        unsafe { self.runs.get_unchecked(class.index()) }
    }

    fn installed_slot(&self) -> NonNull<HeapSlot> {
        let slot = self.slot.get();
        debug_assert!(!slot.is_null());
        // SAFETY: callers reach this only after this TLS heap matched an installed allocator core.
        unsafe { NonNull::new_unchecked(slot) }
    }

    fn return_cached_runs(&self) {
        let Some(slot) = self.slot() else {
            return;
        };

        for run in &self.runs {
            let Some(run) = NonNull::new(run.replace(core::ptr::null_mut())) else {
                continue;
            };

            // SAFETY: slot is installed only while this TLS heap retains the allocator core.
            let _ = unsafe { slot.as_ref() }.return_run(run);
        }
    }

    #[cold]
    fn release_current(&self) {
        self.return_cached_runs();
        let Some(core) = NonNull::new(self.core.replace(core::ptr::null_mut())) else {
            return;
        };
        let heap = self.heap.replace(None);
        self.slot.set(core::ptr::null_mut());

        if let Some(heap) = heap {
            // SAFETY: this TLS heap retained core while installed.
            let core_ref = unsafe { core.as_ref() };
            let mut state = core_ref.state().lock();
            if state.abandon(heap, core_ref.pages()).is_err() {
                abort();
            }
        }

        AllocatorCore::release(core);
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAP: ThreadHeap = const { ThreadHeap::new() };
}

#[cold]
#[inline(never)]
fn abort() -> ! {
    // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
    unsafe { libc::abort() }
}
