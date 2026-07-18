use core::{
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    config::AllocatorConfig,
    heap::{ExtentHeapError, Heap, HeapId, Run, RunHeapError},
    memory::{PageMap, PageOwner},
    size_class::SizeClassId,
    slot_store::{SlotStore, SlotStoreError},
};

use super::remote::RemoteBlocks;

const MAX_HEAPS: usize = 32;
const MAX_HEAPS_U32: u32 = 32;
const HEAP_METADATA_CAPACITY: u32 = 1024;

pub(crate) struct HeapTable {
    slots: SlotStore<HeapSlot>,
    generations: [NonZeroU32; MAX_HEAPS],
    config: AllocatorConfig,
}

// SAFETY: HeapTable is owned by AllocatorState. Slot mutation and remote routing are
// coordinated by the allocator lock; heap internals use their own locks for owner-thread paths.
unsafe impl Send for HeapTable {}

impl HeapTable {
    pub(crate) const fn new(config: AllocatorConfig) -> Self {
        Self {
            slots: SlotStore::new(MAX_HEAPS_U32),
            generations: [NonZeroU32::MIN; MAX_HEAPS],
            config,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<HeapHandle> {
        if let Some(handle) = self.acquire_reusable() {
            return Some(handle);
        }

        let index = self.slots.reserve()?;
        let generation = *self.generations.get(index)?;
        let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
        let slot = HeapSlot::new(id, generation, self.config);

        if self.slots.insert(index, slot).is_err() {
            let _ = self.slots.release(index);
            return None;
        }

        Some(self.slot_mut(id)?.handle(id))
    }

    fn acquire_reusable(&mut self) -> Option<HeapHandle> {
        for index in 0..MAX_HEAPS {
            let Some(slot) = self.slots.get_mut(index) else {
                continue;
            };
            if !slot.is_abandoned() || slot.has_live_allocations() {
                continue;
            }

            let id = HeapId::new(u32::try_from(index).ok()?, slot.generation)?;
            slot.reactivate();
            return Some(slot.handle(id));
        }

        None
    }

    pub(crate) fn active_heap(&self, id: HeapId) -> Option<&Heap> {
        let slot = self.slot(id)?;
        (!slot.is_abandoned()).then_some(&slot.heap)
    }

    pub(crate) fn heap_ref(&self, id: HeapId) -> Result<HeapRef, HeapError> {
        let slot = self.slot(id).ok_or(HeapError::InvalidHeap)?;
        Ok(HeapRef::new(NonNull::from(slot)))
    }

    pub(crate) fn abandon(&mut self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        let slot = self.slot_mut(id).ok_or(HeapError::InvalidHeap)?;
        slot.drain(pages)?;
        slot.abandon();
        Ok(())
    }

    pub(crate) fn reclaim(&mut self, id: HeapId, _pages: &PageMap) -> Result<(), HeapError> {
        let _slot = self.slot_mut(id).ok_or(HeapError::InvalidHeap)?;

        Ok(())
    }

    fn slot_mut(&mut self, id: HeapId) -> Option<&mut HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = self.slots.get_mut(index)?;
        slot.matches(id).then_some(slot)
    }

    fn slot(&self, id: HeapId) -> Option<&HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = self.slots.get(index)?;
        slot.matches(id).then_some(slot)
    }
}

pub(super) struct HeapSlot {
    generation: NonZeroU32,
    abandoned: AtomicBool,
    heap: Heap,
    remote: RemoteBlocks,
}

// SAFETY: HeapSlot is addressable from TLS and remote routing. Its mailbox and heap metadata use
// interior synchronization; table lifecycle changes remain serialized by AllocatorState.
unsafe impl Sync for HeapSlot {}

impl HeapSlot {
    fn new(id: HeapId, generation: NonZeroU32, config: AllocatorConfig) -> Self {
        Self {
            generation,
            abandoned: AtomicBool::new(false),
            heap: Heap::new(id, HEAP_METADATA_CAPACITY, config),
            remote: RemoteBlocks::new(),
        }
    }

    const fn matches(&self, id: HeapId) -> bool {
        self.generation.get() == id.generation().get()
    }

    fn handle(&mut self, id: HeapId) -> HeapHandle {
        HeapHandle::new(id, NonNull::from(self))
    }

    pub(super) fn take_run(&self, class: SizeClassId, pages: &PageMap) -> Option<NonNull<Run>> {
        self.drain(pages).ok()?;
        self.heap.take_available_run(class)
    }

    pub(super) fn allocate_cached_run(&self, run: NonNull<Run>) -> Option<NonNull<u8>> {
        self.heap.allocate_cached_run(run)
    }

    pub(super) fn return_run(&self, run: NonNull<Run>) -> Result<(), HeapError> {
        self.heap.return_run(run).map_err(HeapError::from)
    }

    pub(super) fn free_run(&self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), HeapError> {
        self.heap.free_run(run, ptr).map_err(HeapError::from)
    }

    pub(super) fn free_cached_run(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), HeapError> {
        self.heap.free_cached_run(run, ptr).map_err(HeapError::from)
    }

    fn free_remote_run(&self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), HeapError> {
        Heap::mark_remote_run(run, ptr).map_err(HeapError::from)?;
        self.remote.push(ptr);
        Ok(())
    }

    fn drain(&self, pages: &PageMap) -> Result<(), HeapError> {
        let mut current = self.remote.take_all();
        while let Some(ptr) = current {
            let next = RemoteBlocks::next(ptr);
            let Some(PageOwner::Run(run)) = pages.get(ptr) else {
                return Err(HeapError::InvalidPointer);
            };

            self.heap
                .complete_remote_run(run, ptr)
                .map_err(HeapError::from)?;
            current = next;
        }

        Ok(())
    }

    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }

    fn reactivate(&self) {
        self.abandoned.store(false, Ordering::Release);
    }

    fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    fn has_live_allocations(&self) -> bool {
        self.heap.has_live_allocations()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HeapRef {
    slot: NonNull<HeapSlot>,
}

impl HeapRef {
    fn new(slot: NonNull<HeapSlot>) -> Self {
        Self { slot }
    }

    pub(crate) fn free_run(self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), HeapError> {
        // SAFETY: HeapRef is constructed only from a validated live HeapTable slot.
        unsafe { self.slot.as_ref() }.free_run(run, ptr)
    }

    pub(crate) fn free_remote_run(
        self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), HeapError> {
        // SAFETY: HeapRef is constructed only from a validated live HeapTable slot.
        unsafe { self.slot.as_ref() }.free_remote_run(run, ptr)
    }

    pub(crate) fn is_abandoned(self) -> bool {
        // SAFETY: HeapRef is constructed only from a validated live HeapTable slot.
        unsafe { self.slot.as_ref() }.is_abandoned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    InvalidHeap,
    InvalidPointer,
    DoubleFree,
    InvalidMetadata,
}

impl From<RunHeapError> for HeapError {
    fn from(error: RunHeapError) -> Self {
        match error {
            RunHeapError::InvalidPointer => Self::InvalidPointer,
            RunHeapError::DoubleFree => Self::DoubleFree,
            RunHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<ExtentHeapError> for HeapError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent | ExtentHeapError::InvalidMetadata => {
                Self::InvalidMetadata
            }
            ExtentHeapError::InvalidPointer => Self::InvalidPointer,
        }
    }
}

impl From<SlotStoreError> for HeapError {
    fn from(_: SlotStoreError) -> Self {
        Self::InvalidMetadata
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HeapHandle {
    id: HeapId,
    slot: NonNull<HeapSlot>,
}

impl HeapHandle {
    fn new(id: HeapId, slot: NonNull<HeapSlot>) -> Self {
        Self { id, slot }
    }

    pub(crate) const fn id(self) -> HeapId {
        self.id
    }

    pub(super) const fn slot_ptr(self) -> NonNull<HeapSlot> {
        self.slot
    }
}
