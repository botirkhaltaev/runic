use core::{num::NonZeroU32, ptr::NonNull};

use crate::{
    arena::Arena,
    config::AllocatorConfig,
    heap::{ExtentHeapError, Heap, HeapId, RunHeapError},
    memory::PageMap,
};

use super::inbox::RemoteList;

const MAX_HEAPS: usize = 64;
const MAX_HEAPS_U32: u32 = 64;
const HEAP_METADATA_CAPACITY: u32 = 16_384;

pub(crate) struct HeapTable {
    heaps: Arena<Heap>,
    generations: [NonZeroU32; MAX_HEAPS],
    config: AllocatorConfig,
}

// SAFETY: HeapTable is owned by AllocatorState. Slot mutation and remote routing are
// coordinated by the allocator lock; heap internals require exclusive owner or table access.
unsafe impl Send for HeapTable {}

impl HeapTable {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            heaps: Arena::new(MAX_HEAPS_U32),
            generations: [NonZeroU32::MIN; MAX_HEAPS],
            config,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<NonNull<Heap>> {
        if let Some(heap) = self.acquire_reusable() {
            return Some(heap);
        }

        let index = self.heaps.claim()?;
        let generation = *self.generations.get(index)?;
        let Some(id) = HeapId::new(u32::try_from(index).ok()?, generation) else {
            self.heaps.release(index);
            return None;
        };
        let heap = Heap::new(id, HEAP_METADATA_CAPACITY, self.config);

        if self.heaps.insert(index, heap).is_none() {
            self.heaps.release(index);
            return None;
        }

        self.heaps.get_mut(index).map(NonNull::from)
    }

    fn acquire_reusable(&mut self) -> Option<NonNull<Heap>> {
        for index in 0..MAX_HEAPS {
            let Some(heap) = self.heaps.get_mut(index) else {
                continue;
            };
            if !heap.is_free() {
                continue;
            }

            let generation = *self.generations.get(index)?;
            let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
            heap.reactivate(id);
            return Some(NonNull::from(heap));
        }

        None
    }

    pub(crate) fn get(&self, id: HeapId) -> Option<&Heap> {
        let index = usize::try_from(id.index()).ok()?;
        let heap = self.heaps.get(index)?;
        self.matches_generation(index, id).then_some(heap)
    }

    pub(crate) fn get_mut(&mut self, id: HeapId) -> Option<&mut Heap> {
        let index = usize::try_from(id.index()).ok()?;
        if !self.matches_generation(index, id) {
            return None;
        }
        self.heaps.get_mut(index)
    }

    pub(crate) fn push_remote_batch(&self, id: HeapId, list: &RemoteList) -> Result<(), HeapError> {
        let heap = self.get(id).ok_or(HeapError::InvalidHeap)?;
        if !heap.is_active() {
            return Err(HeapError::InvalidHeap);
        }

        heap.inbox().push_batch(list);
        Ok(())
    }

    pub(crate) fn release_heap(&mut self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        let index = usize::try_from(id.index())
            .ok()
            .ok_or(HeapError::InvalidHeap)?;
        let reclaimed = {
            let heap = self.get_mut(id).ok_or(HeapError::InvalidHeap)?;
            heap.flush(pages)?;
            heap.begin_drain();
            heap.flush(pages)?;
            heap.try_reclaim()
        };
        if reclaimed {
            self.bump_generation(index);
        }
        Ok(())
    }

    pub(crate) fn try_reclaim_heap(&mut self, id: HeapId) -> bool {
        let Some(index) = usize::try_from(id.index()).ok() else {
            return false;
        };
        let Some(heap) = self.get_mut(id) else {
            return false;
        };
        if !heap.try_reclaim() {
            return false;
        }
        self.bump_generation(index);
        true
    }

    fn matches_generation(&self, index: usize, id: HeapId) -> bool {
        self.generations
            .get(index)
            .is_some_and(|generation| *generation == id.generation())
    }

    fn bump_generation(&mut self, index: usize) {
        let Some(stored) = self.generations.get_mut(index) else {
            return;
        };
        if let Some(next) = stored.get().checked_add(1).and_then(NonZeroU32::new) {
            *stored = next;
        }
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
            ExtentHeapError::DoubleFree => Self::DoubleFree,
        }
    }
}
