//! Grow-on-demand mmap object table with an intrusive freelist.
//!
//! Indices are `u32` end-to-end (matching `HeapId` / `RunId` / `ExtentId`). Use `usize`
//! only when indexing Rust arrays or doing pointer/byte math.
//!
//! `new(max)` records a hard index limit only — no slots are mapped until
//! `claim`. Growth appends fixed-size slot chunks, each a normal [`Mapping`]
//! owned by [`Chunk`]. Indices are stable for the arena lifetime. Callers that
//! fail after `claim` and before `insert` must `release` the index.

use core::{mem::MaybeUninit, ptr::NonNull};

use crate::memory::{Mapping, OsMemory};

const FREE_END: u32 = u32::MAX;

/// Target bytes of slot storage per growth step (page-rounded by [`OsMemory::map`]).
const CHUNK_BYTES: usize = 256 * 1024;

/// In-struct chunk directory. Sized for `SLOT_CAPACITY` at modest slot
/// sizes while keeping `Arena` small enough to embed in a heap slot.
const MAX_CHUNKS: u32 = 32;
const MAX_CHUNKS_LEN: usize = 32;
const _: () = assert!(MAX_CHUNKS == 32 && MAX_CHUNKS_LEN == 32);

pub(crate) struct Arena<T> {
    max: u32,
    /// One past the highest index ever handed out by bump `claim`.
    bump: u32,
    free_head: u32,
    slots_per_chunk: u32,
    chunk_count: u32,
    chunks: [Option<Chunk<T>>; MAX_CHUNKS_LEN],
}

/// One slot group: sole owner of its mmap and a typed pointer to its slots.
struct Chunk<T> {
    mapping: Mapping,
    slots: NonNull<Slot<T>>,
    len: u32,
}

// SAFETY: Arena owns mmap-backed storage. Moving ownership does not permit concurrent mutation.
unsafe impl<T: Send> Send for Arena<T> {}

impl<T> Arena<T> {
    pub(crate) fn new(max: u32) -> Self {
        let slots_per_chunk = Self::slots_per_chunk();
        let max_supported = slots_per_chunk.saturating_mul(MAX_CHUNKS);
        let max = max.min(max_supported);

        Self {
            max,
            bump: 0,
            free_head: FREE_END,
            slots_per_chunk,
            chunk_count: 0,
            chunks: [const { None }; MAX_CHUNKS_LEN],
        }
    }

    pub(crate) fn claim(&mut self) -> Option<u32> {
        if self.free_head != FREE_END {
            let index = self.free_head;
            let next = {
                let slot = self.slot_mut(index)?;
                debug_assert!(!slot.is_occupied());
                slot.take_next()
            };
            self.free_head = next;
            return Some(index);
        }

        if self.bump >= self.max {
            return None;
        }

        self.ensure_chunk(self.bump)?;
        let index = self.bump;
        self.bump += 1;
        Some(index)
    }

    pub(crate) fn release(&mut self, index: u32) {
        let next = self.free_head;
        let Some(slot) = self.slot_mut(index) else {
            return;
        };
        if slot.is_occupied() {
            return;
        }

        slot.set_empty(next);
        self.free_head = index;
    }

    pub(crate) fn insert(&mut self, index: u32, value: T) -> Option<&mut T> {
        let slot = self.slot_mut(index)?;
        if slot.is_occupied() {
            return None;
        }

        slot.occupy(value);
        slot.get_mut()
    }

    pub(crate) fn get(&self, index: u32) -> Option<&T> {
        self.slot(index)?.get()
    }

    pub(crate) fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        self.slot_mut(index)?.get_mut()
    }

    pub(crate) fn remove(&mut self, index: u32) -> Option<T> {
        let value = self.slot_mut(index)?.remove()?;
        self.release(index);
        Some(value)
    }

    /// Hard index limit passed to [`Self::new`] (after directory support clamp).
    pub(crate) fn capacity(&self) -> u32 {
        self.max
    }

    /// Number of indices ever committed by bump growth.
    pub(crate) fn len(&self) -> u32 {
        self.bump
    }

    fn slots_per_chunk() -> u32 {
        let slot = core::mem::size_of::<Slot<T>>().max(1);
        debug_assert!(core::mem::align_of::<Slot<T>>() <= crate::memory::PAGE_SIZE);
        let n = (CHUNK_BYTES / slot).max(1);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn ensure_chunk(&mut self, index: u32) -> Option<()> {
        let chunk_index = index / self.slots_per_chunk;
        let slot = self.chunks.get_mut(usize::try_from(chunk_index).ok()?)?;
        if slot.is_some() {
            return Some(());
        }
        if chunk_index != self.chunk_count {
            return None;
        }

        let start = chunk_index.checked_mul(self.slots_per_chunk)?;
        if start >= self.max {
            return None;
        }
        let len = self.slots_per_chunk.min(self.max - start);
        let byte_len = usize::try_from(len)
            .ok()?
            .checked_mul(core::mem::size_of::<Slot<T>>())?;
        let mapping = OsMemory::map(byte_len)?;
        debug_assert!(mapping.len().get() >= byte_len);
        let slots = mapping.base().cast::<Slot<T>>();

        *slot = Some(Chunk {
            mapping,
            slots,
            len,
        });
        self.chunk_count += 1;
        Some(())
    }

    fn slot(&self, index: u32) -> Option<&Slot<T>> {
        let ptr = self.slot_ptr(index)?;
        // SAFETY: `slot_ptr` yields a live slot inside an owned chunk mapping.
        Some(unsafe { ptr.as_ref() })
    }

    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T>> {
        let mut ptr = self.slot_ptr(index)?;
        // SAFETY: `slot_ptr` yields a live slot inside an owned chunk mapping; Arena is uniquely borrowed.
        Some(unsafe { ptr.as_mut() })
    }

    fn slot_ptr(&self, index: u32) -> Option<NonNull<Slot<T>>> {
        if index >= self.bump {
            return None;
        }

        let chunk_index = index / self.slots_per_chunk;
        let offset = index % self.slots_per_chunk;
        let chunk = self
            .chunks
            .get(usize::try_from(chunk_index).ok()?)?
            .as_ref()?;
        if offset >= chunk.len {
            return None;
        }

        debug_assert_eq!(
            chunk.slots.as_ptr().cast::<u8>(),
            chunk.mapping.base().as_ptr()
        );

        let offset = usize::try_from(offset).ok()?;
        // SAFETY: `slots` points at `chunk.len` slots in `mapping`; `offset` is in range.
        Some(unsafe { NonNull::new_unchecked(chunk.slots.as_ptr().add(offset)) })
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        // Drop occupied values before chunk mappings munmap the backing pages.
        for index in 0..self.bump {
            if let Some(slot) = self.slot_mut(index) {
                slot.drop_value();
            }
        }
        for chunk in &mut self.chunks {
            if let Some(chunk) = chunk.take() {
                drop(chunk.mapping);
            }
        }
    }
}

#[repr(C)]
struct Slot<T> {
    value: MaybeUninit<T>,
    /// Freelist next when empty; unused when occupied.
    next: u32,
    occupied: u8,
}

impl<T> Slot<T> {
    fn set_empty(&mut self, next: u32) {
        self.occupied = 0;
        self.next = next;
    }

    fn take_next(&mut self) -> u32 {
        debug_assert!(!self.is_occupied());
        self.next
    }

    fn is_occupied(&self) -> bool {
        self.occupied != 0
    }

    fn occupy(&mut self, value: T) {
        debug_assert!(!self.is_occupied());
        self.value.write(value);
        self.occupied = 1;
        self.next = FREE_END;
    }

    fn get_mut(&mut self) -> Option<&mut T> {
        if !self.is_occupied() {
            return None;
        }
        // SAFETY: occupied is set only after value.write.
        Some(unsafe { self.value.assume_init_mut() })
    }

    fn get(&self) -> Option<&T> {
        if !self.is_occupied() {
            return None;
        }
        // SAFETY: occupied is set only after value.write.
        Some(unsafe { self.value.assume_init_ref() })
    }

    fn remove(&mut self) -> Option<T> {
        if !self.is_occupied() {
            return None;
        }
        self.occupied = 0;
        // SAFETY: occupied was true, so value is initialized.
        Some(unsafe { self.value.assume_init_read() })
    }

    fn drop_value(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct DropCounter<'a> {
        drops: &'a Cell<usize>,
    }

    impl Drop for DropCounter<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn arena_zero_capacity_claims_none() {
        let mut arena = Arena::<u32>::new(0);
        assert_eq!(arena.claim(), None);
    }

    #[test]
    fn arena_claims_from_zero() {
        let mut arena = Arena::<u32>::new(4);
        assert_eq!(arena.claim(), Some(0));
        assert_eq!(arena.claim(), Some(1));
    }

    #[test]
    fn arena_respects_capacity() {
        let mut arena = Arena::<u32>::new(2);
        assert_eq!(arena.claim(), Some(0));
        assert_eq!(arena.claim(), Some(1));
        assert_eq!(arena.claim(), None);
    }

    #[test]
    fn arena_release_returns_index_to_freelist() {
        let mut arena = Arena::<u32>::new(4);
        let first = arena.claim().unwrap();
        let second = arena.claim().unwrap();
        arena.release(first);
        assert_eq!(second, 1);
        assert_eq!(arena.claim(), Some(first));
    }

    #[test]
    fn arena_insert_get_remove_round_trip() {
        let mut arena = Arena::<u32>::new(4);
        let index = arena.claim().unwrap();
        assert_eq!(arena.insert(index, 42).copied(), Some(42));
        assert_eq!(arena.get(index).copied(), Some(42));
        assert_eq!(arena.remove(index), Some(42));
        assert_eq!(arena.get(index), None);
        assert_eq!(arena.claim(), Some(index));
    }

    #[test]
    fn arena_rejects_insert_on_occupied() {
        let mut arena = Arena::<u32>::new(4);
        let index = arena.claim().unwrap();
        assert!(arena.insert(index, 1).is_some());
        assert!(arena.insert(index, 2).is_none());
        assert_eq!(arena.get(index).copied(), Some(1));
    }

    #[test]
    fn arena_drop_drops_occupied_only() {
        let drops = Cell::new(0);
        {
            let mut arena = Arena::new(4);
            let occupied = arena.claim().unwrap();
            let claimed = arena.claim().unwrap();
            assert!(
                arena
                    .insert(occupied, DropCounter { drops: &drops })
                    .is_some()
            );
            assert_ne!(occupied, claimed);
            // `claimed` is released by dropping the arena without insert — no DropCounter.
        }
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn arena_grows_across_chunks_without_eager_len() {
        let mut arena = Arena::<u32>::new(4_096);
        assert_eq!(arena.len(), 0);

        let first = arena.claim().unwrap();
        assert_eq!(first, 0);
        assert_eq!(arena.len(), 1);
        assert!(arena.insert(first, 7).is_some());

        let limit = arena.capacity().min(arena.slots_per_chunk + 1);
        while arena.len() < limit {
            let index = arena.claim().unwrap();
            assert!(arena.insert(index, index).is_some());
        }
        assert_eq!(arena.len(), limit);
        assert_eq!(arena.get(0).copied(), Some(7));
        assert_eq!(arena.get(limit - 1).copied(), Some(limit - 1));
        assert!(arena.chunk_count >= 2 || limit <= arena.slots_per_chunk);
    }

    #[test]
    fn arena_each_chunk_owns_a_mapping() {
        // Large elements force small slots_per_chunk so two chunks fit under max.
        #[repr(C)]
        struct Large([u8; 4096]);

        let mut arena = Arena::<Large>::new(64);
        let need = arena.slots_per_chunk + 1;
        assert!(need <= arena.capacity());
        while arena.len() < need {
            let index = arena.claim().unwrap();
            assert!(arena.insert(index, Large([0; 4096])).is_some());
        }
        assert_eq!(arena.chunk_count, 2);
        assert!(arena.chunks[0].as_ref().unwrap().mapping.len().get() > 0);
        assert!(arena.chunks[1].as_ref().unwrap().mapping.len().get() > 0);
    }
}
