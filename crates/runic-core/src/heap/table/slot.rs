use core::{num::NonZeroU32, ptr::NonNull};

use crate::{
    arena::Arena,
    config::AllocatorConfig,
    heap::{ExtentHeapError, Heap, HeapId, HeapMode, RunHeapError},
    memory::PageMap,
};

use super::inbox::RemoteList;

const MAX_HEAPS: usize = 64;
const MAX_HEAPS_U32: u32 = 64;
const HEAP_METADATA_CAPACITY: u32 = 16_384;

/// Generation-checked heap slots and remote-batch delivery.
pub(crate) struct HeapTable {
    slots: Arena<Heap>,
    generations: [NonZeroU32; MAX_HEAPS],
    config: AllocatorConfig,
}

// SAFETY: HeapTable is owned under AllocatorInner's table mutex. Slot mutation and remote
// routing are coordinated by that lock; heap internals require exclusive owner or table access.
unsafe impl Send for HeapTable {}

impl HeapTable {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            slots: Arena::new(MAX_HEAPS_U32),
            generations: [NonZeroU32::MIN; MAX_HEAPS],
            config,
        }
    }

    /// Acquire a heap for TLS bind: reuse a Free slot or claim a fresh one.
    pub(crate) fn acquire(&mut self) -> Option<(HeapId, NonNull<Heap>)> {
        if let Some(acquired) = self.acquire_reusable() {
            return Some(acquired);
        }

        let index = self.slots.claim()?;
        let generation = *self.generations.get(index)?;
        let Some(id) = HeapId::new(u32::try_from(index).ok()?, generation) else {
            self.slots.release(index);
            return None;
        };
        let heap = Heap::new(id, HEAP_METADATA_CAPACITY, self.config);

        if self.slots.insert(index, heap).is_none() {
            self.slots.release(index);
            return None;
        }

        let heap = NonNull::from(self.slots.get_mut(index)?);
        // SAFETY: heap was just inserted into a live table slot.
        Some((unsafe { heap.as_ref().id() }, heap))
    }

    fn acquire_reusable(&mut self) -> Option<(HeapId, NonNull<Heap>)> {
        for index in 0..MAX_HEAPS {
            let Some(heap) = self.slots.get_mut(index) else {
                continue;
            };
            if !heap.is_free() {
                continue;
            }

            let generation = *self.generations.get(index)?;
            let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
            heap.reactivate(id);
            let heap = NonNull::from(heap);
            // SAFETY: heap is a live Free slot just reactivated in this table.
            return Some((unsafe { heap.as_ref().id() }, heap));
        }

        None
    }

    /// Generation-checked shared borrow of a live heap.
    pub(crate) fn heap(&self, id: HeapId) -> Option<&Heap> {
        let index = usize::try_from(id.index()).ok()?;
        let heap = self.slots.get(index)?;
        self.matches_generation(index, id).then_some(heap)
    }

    /// Generation-checked exclusive borrow of a live heap.
    pub(crate) fn heap_mut(&mut self, id: HeapId) -> Option<&mut Heap> {
        let index = usize::try_from(id.index()).ok()?;
        if !self.matches_generation(index, id) {
            return None;
        }
        self.slots.get_mut(index)
    }

    /// Lifecycle mode for a live `HeapId`, or `None` if missing/stale.
    pub(crate) fn mode(&self, id: HeapId) -> Option<HeapMode> {
        self.heap(id).map(Heap::mode)
    }

    /// Publish a claimed remote-free batch to `id`.
    ///
    /// - `Active`: enqueue onto the heap inbox (owner flushes later).
    /// - `Draining`: enqueue then complete under the table lock, then reclaim if empty.
    /// - `Free` / stale generation: error (caller must not drop claimed nodes).
    pub(crate) fn publish(
        &mut self,
        id: HeapId,
        list: &RemoteList,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        match self.mode(id).ok_or(HeapError::InvalidHeap)? {
            HeapMode::Active => {
                self.heap(id)
                    .ok_or(HeapError::InvalidHeap)?
                    .inbox()
                    .push_batch(list);
                Ok(())
            }
            HeapMode::Draining => {
                {
                    let heap = self.heap_mut(id).ok_or(HeapError::InvalidHeap)?;
                    heap.inbox().push_batch(list);
                    heap.flush(pages)?;
                }
                let _ = self.reclaim(id);
                Ok(())
            }
            HeapMode::Free => Err(HeapError::InvalidHeap),
        }
    }

    /// Owner thread gives up the heap: flush, enter Draining, flush again, reclaim if empty.
    pub(crate) fn retire(&mut self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        let reclaimed = {
            let heap = self.heap_mut(id).ok_or(HeapError::InvalidHeap)?;
            heap.flush(pages)?;
            heap.begin_drain();
            heap.flush(pages)?;
            heap.try_reclaim()
        };
        if reclaimed {
            self.bump_generation(id);
        }
        Ok(())
    }

    /// If the heap is empty under Draining, mark Free and bump its generation.
    pub(crate) fn reclaim(&mut self, id: HeapId) -> bool {
        let Some(heap) = self.heap_mut(id) else {
            return false;
        };
        if !heap.try_reclaim() {
            return false;
        }
        self.bump_generation(id);
        true
    }

    fn matches_generation(&self, index: usize, id: HeapId) -> bool {
        self.generations
            .get(index)
            .is_some_and(|generation| *generation == id.generation())
    }

    fn bump_generation(&mut self, id: HeapId) {
        let Ok(index) = usize::try_from(id.index()) else {
            return;
        };
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
