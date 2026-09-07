//! Grow-on-demand mmap slab. A slot is vacant or occupied.
//! [`Arena::vacant`] is the next vacant index (maps storage, does not occupy).
//! [`Arena::insert`] occupies that index; [`Arena::remove`] returns it to vacant.
//! Mutation is `&mut self`.
//!
//! Indices are `u32` end-to-end (matching `HeapId` / `RunId` / `ExtentId`). Use `usize`
//! only when indexing Rust arrays or doing pointer/byte math.
//!
//! Growth appends fixed-size slot chunks and doubles an mmap-backed descriptor
//! directory like `Vec<Box<[Slot<T>]>>`. The directory moves; occupied slots do not.
//! Sharing is the caller's lock (`RwLock<Arena<T>>`), not interior atomics.

use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
};

use crate::memory::{Mapping, OsMemory, PAGE_SIZE};

const VACANT_END: u32 = u32::MAX;

/// Target bytes of slot storage per growth step (page-rounded by [`OsMemory::map`]).
const CHUNK_BYTES: usize = 256 * 1024;

pub(crate) struct Arena<T> {
    /// One past the highest index ever occupied by bump insert.
    bump: u32,
    vacant_head: u32,
    slots_per_chunk: u32,
    chunk_count: u32,
    directory: Option<Directory<T>>,
}

/// One slot group: sole owner of its mmap and a typed pointer to its slots.
struct Chunk<T> {
    mapping: Mapping,
    slots: NonNull<Slot<T>>,
}

/// Movable descriptor storage. Descriptors move on growth; their slot mappings do not.
struct Directory<T> {
    mapping: Mapping,
    chunks: NonNull<Chunk<T>>,
    capacity: u32,
}

// SAFETY: Arena owns mmap-backed storage. Moving ownership does not permit concurrent mutation.
unsafe impl<T: Send> Send for Arena<T> {}

impl<T> Arena<T> {
    pub(crate) fn new() -> Self {
        Self {
            bump: 0,
            vacant_head: VACANT_END,
            slots_per_chunk: Self::slots_per_chunk(),
            chunk_count: 0,
            directory: None,
        }
    }

    /// Next vacant index. Maps the chunk if this is a new bump slot.
    /// Does not occupy; a second call returns the same index until [`Self::insert`].
    pub(crate) fn vacant(&mut self) -> Option<u32> {
        if self.vacant_head != VACANT_END {
            return Some(self.vacant_head);
        }
        if self.bump == u32::MAX {
            return None;
        }
        self.ensure_chunk(self.bump)?;
        Some(self.bump)
    }

    /// Occupy `index` from [`Self::vacant`]. Any other index is rejected.
    pub(crate) fn insert(&mut self, index: u32, value: T) -> Option<&mut T> {
        if self.vacant_head != VACANT_END {
            if index != self.vacant_head {
                return None;
            }
            let next = match self.slot(index)? {
                Slot::Vacant { next } => *next,
                Slot::Occupied(_) => return None,
            };
            self.vacant_head = next;
            *self.slot_mut(index)? = Slot::Occupied(value);
            return self.get_mut(index);
        }

        if index != self.bump {
            return None;
        }
        if self.bump == u32::MAX {
            return None;
        }
        self.ensure_chunk(index)?;
        // SAFETY: ensure_chunk mapped this bump slot; it has never been initialized.
        unsafe { self.slot_ptr_unchecked(index).write(Slot::Occupied(value)) };
        self.bump += 1;
        self.get_mut(index)
    }

    pub(crate) fn get(&self, index: u32) -> Option<&T> {
        match self.slot(index)? {
            Slot::Vacant { .. } => None,
            Slot::Occupied(value) => Some(value),
        }
    }

    pub(crate) fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        match self.slot_mut(index)? {
            Slot::Vacant { .. } => None,
            Slot::Occupied(value) => Some(value),
        }
    }

    pub(crate) fn remove(&mut self, index: u32) -> Option<T> {
        let next = self.vacant_head;
        let slot = self.slot_mut(index)?;
        if matches!(slot, Slot::Vacant { .. }) {
            return None;
        }
        let Slot::Occupied(value) = mem::replace(slot, Slot::Vacant { next }) else {
            unreachable!();
        };
        self.vacant_head = index;
        Some(value)
    }

    pub(crate) fn iter(&self) -> Iter<'_, T> {
        Iter {
            arena: self,
            index: 0,
        }
    }

    pub(crate) fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut {
            arena: NonNull::from(self),
            index: 0,
            marker: PhantomData,
        }
    }

    fn slots_per_chunk() -> u32 {
        let slot = core::mem::size_of::<Slot<T>>().max(1);
        debug_assert!(core::mem::align_of::<Slot<T>>() <= crate::memory::PAGE_SIZE);
        let n = (CHUNK_BYTES / slot).max(1);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn ensure_chunk(&mut self, index: u32) -> Option<()> {
        let chunk_index = index / self.slots_per_chunk;
        if chunk_index < self.chunk_count {
            return Some(());
        }
        if chunk_index != self.chunk_count {
            return None;
        }

        self.ensure_directory(chunk_index.checked_add(1)?)?;
        let byte_len = usize::try_from(self.slots_per_chunk)
            .ok()?
            .checked_mul(core::mem::size_of::<Slot<T>>())?;
        let mapping = OsMemory::map(byte_len)?;
        debug_assert!(mapping.len().get() >= byte_len);
        let slots = mapping.base().cast::<Slot<T>>();

        let directory = self.directory.as_mut()?;
        // SAFETY: ensure_directory provides capacity for chunk_index; each descriptor is
        // written exactly once before chunk_count publishes it.
        unsafe {
            directory
                .chunks
                .as_ptr()
                .add(usize::try_from(chunk_index).ok()?)
                .write(Chunk { mapping, slots });
        }
        self.chunk_count += 1;
        Some(())
    }

    fn ensure_directory(&mut self, needed: u32) -> Option<()> {
        let Some(directory) = &mut self.directory else {
            self.directory = Some(Directory::new()?);
            return self
                .directory
                .as_ref()
                .filter(|directory| directory.capacity >= needed)
                .map(|_| ());
        };
        if needed <= directory.capacity {
            return Some(());
        }
        directory.grow(self.chunk_count, needed)
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
        if chunk_index >= self.chunk_count {
            return None;
        }
        let directory = self.directory.as_ref()?;
        // SAFETY: chunk_index is below published chunk_count, so the descriptor is initialized.
        let chunk = unsafe {
            directory
                .chunks
                .as_ptr()
                .add(usize::try_from(chunk_index).ok()?)
                .as_ref()?
        };

        debug_assert_eq!(
            chunk.slots.as_ptr().cast::<u8>(),
            chunk.mapping.base().as_ptr()
        );

        let offset = usize::try_from(offset).ok()?;
        // SAFETY: `slots` points at `slots_per_chunk` slots in `mapping`; `offset` is in range.
        Some(unsafe { NonNull::new_unchecked(chunk.slots.as_ptr().add(offset)) })
    }
}

impl<T> Directory<T> {
    fn new() -> Option<Self> {
        let mapping = OsMemory::map(PAGE_SIZE)?;
        let capacity = u32::try_from(mapping.len().get() / mem::size_of::<Chunk<T>>()).ok()?;
        if capacity == 0 {
            return None;
        }
        let chunks = mapping.base().cast::<Chunk<T>>();
        Some(Self {
            mapping,
            chunks,
            capacity,
        })
    }

    fn grow(&mut self, initialized: u32, needed: u32) -> Option<()> {
        let mut capacity = self.capacity;
        while capacity < needed {
            capacity = capacity.checked_mul(2)?;
        }
        let byte_len = usize::try_from(capacity)
            .ok()?
            .checked_mul(mem::size_of::<Chunk<T>>())?;
        let mapping = OsMemory::map(byte_len)?;
        let chunks = mapping.base().cast::<Chunk<T>>();
        // SAFETY: source has initialized descriptors through initialized; destination is
        // disjoint mapped storage with sufficient alignment and capacity. This moves each
        // Mapping owner bitwise; source descriptors are never dropped before their mmap goes.
        unsafe {
            ptr::copy_nonoverlapping(
                self.chunks.as_ptr(),
                chunks.as_ptr(),
                usize::try_from(initialized).ok()?,
            );
        }
        let old = mem::replace(&mut self.mapping, mapping);
        self.chunks = chunks;
        self.capacity = capacity;
        drop(old);
        Some(())
    }
}

pub(crate) struct Iter<'a, T> {
    arena: &'a Arena<T>,
    index: u32,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.arena.bump {
            let index = self.index;
            self.index += 1;
            if let Some(value) = self.arena.get(index) {
                return Some(value);
            }
        }
        None
    }
}

pub(crate) struct IterMut<'a, T> {
    arena: NonNull<Arena<T>>,
    index: u32,
    marker: PhantomData<&'a mut Arena<T>>,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: IterMut owns the exclusive Arena borrow for 'a. Indices increase
        // monotonically, so each occupied slot is yielded at most once.
        let arena = unsafe { self.arena.as_mut() };
        while self.index < arena.bump {
            let index = self.index;
            self.index += 1;
            if let Some(value) = arena.get_mut(index) {
                let value = NonNull::from(value);
                // SAFETY: this slot has not been yielded before and remains stable.
                return Some(unsafe { &mut *value.as_ptr() });
            }
        }
        None
    }
}

enum Slot<T> {
    Vacant { next: u32 },
    Occupied(T),
}

impl<T> Arena<T> {
    unsafe fn slot_ptr_unchecked(&self, index: u32) -> *mut Slot<T> {
        let chunk_index = index / self.slots_per_chunk;
        let offset = index % self.slots_per_chunk;
        let directory = self
            .directory
            .as_ref()
            .expect("mapped slot must have a directory");
        // SAFETY: caller guarantees the chunk and slot are mapped.
        let chunk = unsafe {
            &*directory
                .chunks
                .as_ptr()
                .add(usize::try_from(chunk_index).expect("u32 fits usize"))
        };
        // SAFETY: caller guarantees offset is in this fixed-size chunk.
        unsafe {
            chunk
                .slots
                .as_ptr()
                .add(usize::try_from(offset).expect("u32 fits usize"))
        }
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        for index in 0..self.bump {
            if let Some(slot) = self.slot_mut(index) {
                // SAFETY: every committed slot contains a valid Slot<T>.
                unsafe { ptr::drop_in_place(slot) };
            }
        }
        let Some(directory) = &self.directory else {
            return;
        };
        for index in 0..self.chunk_count {
            // SAFETY: descriptors below chunk_count are initialized and uniquely owned.
            unsafe {
                ptr::drop_in_place(
                    directory
                        .chunks
                        .as_ptr()
                        .add(usize::try_from(index).expect("u32 fits usize")),
                );
            }
        }
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

    fn occupy(arena: &mut Arena<u32>, value: u32) -> u32 {
        let index = arena.vacant().unwrap();
        arena.insert(index, value).unwrap();
        index
    }

    #[test]
    fn arena_vacant_assigns_indices_from_zero() {
        let mut arena = Arena::<u32>::new();
        assert_eq!(occupy(&mut arena, 10), 0);
        assert_eq!(occupy(&mut arena, 20), 1);
        assert_eq!(arena.get(0).copied(), Some(10));
        assert_eq!(arena.get(1).copied(), Some(20));
    }

    #[test]
    fn arena_vacant_without_insert_consumes_nothing() {
        let mut arena = Arena::<u32>::new();
        assert_eq!(arena.vacant(), Some(0));
        assert_eq!(arena.vacant(), Some(0));
    }

    #[test]
    fn arena_insert_rejects_index_that_is_not_vacant() {
        let mut arena = Arena::<u32>::new();
        let index = arena.vacant().unwrap();
        assert!(arena.insert(index + 1, 1).is_none());
        assert_eq!(arena.vacant(), Some(index));
    }

    #[test]
    fn arena_remove_returns_index_to_vacant() {
        let mut arena = Arena::<u32>::new();
        let first = occupy(&mut arena, 10);
        assert_eq!(occupy(&mut arena, 20), 1);
        assert_eq!(arena.remove(first), Some(10));
        assert_eq!(arena.vacant(), Some(first));
        assert_eq!(occupy(&mut arena, 30), first);
    }

    #[test]
    fn arena_insert_get_remove_round_trip() {
        let mut arena = Arena::<u32>::new();
        let index = occupy(&mut arena, 42);
        assert_eq!(arena.get(index).copied(), Some(42));
        assert_eq!(arena.remove(index), Some(42));
        assert_eq!(arena.get(index), None);
        assert_eq!(occupy(&mut arena, 7), index);
    }

    #[test]
    fn arena_iterators_yield_occupied_only() {
        let mut arena = Arena::<u32>::new();
        for value in 1..=3 {
            occupy(&mut arena, value);
        }
        assert_eq!(arena.remove(1), Some(2));
        assert_eq!(arena.iter().copied().collect::<Vec<_>>(), vec![1, 3]);
        for value in arena.iter_mut() {
            *value *= 2;
        }
        assert_eq!(arena.iter().copied().collect::<Vec<_>>(), vec![2, 6]);
    }

    #[test]
    fn arena_drop_drops_occupied_only() {
        let drops = Cell::new(0);
        {
            let mut arena = Arena::new();
            let first = arena.vacant().unwrap();
            arena.insert(first, DropCounter { drops: &drops }).unwrap();
            let removed = arena.vacant().unwrap();
            arena
                .insert(removed, DropCounter { drops: &drops })
                .unwrap();
            drop(arena.remove(removed));
            assert_eq!(arena.vacant(), Some(removed));
        }
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn arena_grows_past_thirty_two_chunks() {
        #[repr(C)]
        struct Large([u8; 4096]);

        let mut arena = Arena::<Large>::new();
        let entries = arena.slots_per_chunk * 32 + 1;
        for index in 0..entries {
            let mut value = Large([0; 4096]);
            value.0[..4].copy_from_slice(&index.to_le_bytes());
            let slot = arena.vacant().unwrap();
            arena.insert(slot, value).unwrap();
        }

        assert_eq!(arena.chunk_count, 33);
        assert_eq!(&arena.get(0).unwrap().0[..4], &0u32.to_le_bytes());
        assert_eq!(
            &arena.get(entries - 1).unwrap().0[..4],
            &(entries - 1).to_le_bytes()
        );
    }

    #[test]
    fn arena_directory_grows_without_moving_slots() {
        #[repr(C)]
        struct Large([u8; 4096]);

        let mut arena = Arena::<Large>::new();
        let first_index = arena.vacant().unwrap();
        arena.insert(first_index, Large([0; 4096])).unwrap();
        let first = NonNull::from(arena.get(0).unwrap());
        let initial_capacity = arena.directory.as_ref().unwrap().capacity;
        let entries = initial_capacity
            .checked_mul(arena.slots_per_chunk)
            .and_then(|value| value.checked_add(1))
            .unwrap();

        for index in 1..entries {
            let mut value = Large([0; 4096]);
            value.0[..4].copy_from_slice(&index.to_le_bytes());
            let slot = arena.vacant().unwrap();
            arena.insert(slot, value).unwrap();
        }

        assert!(arena.directory.as_ref().unwrap().capacity > initial_capacity);
        assert_eq!(NonNull::from(arena.get(0).unwrap()), first);
        assert_eq!(arena.iter().count(), usize::try_from(entries).unwrap());
        assert_eq!(
            &arena.get(entries - 1).unwrap().0[..4],
            &(entries - 1).to_le_bytes()
        );
    }
}
