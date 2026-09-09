//! Published immovable-slot arena.
//!
//! [`Arena::get`] is a lock-free directory read (`len` Acquire, chunk pointer
//! Acquire). Occupied slots never move or unmap. Growth maps another 256 KiB
//! chunk under the grow mutex.
//!
//! Exclusive holes: [`Arena::vacant`] / [`Arena::insert`] / [`Arena::remove`] (`&mut self`).
//! Shared append: [`Arena::push`] (`&self`).

use core::{
    marker::PhantomData,
    mem::size_of,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::memory::{Mapping, OsMemory, PAGE_SIZE};

/// Target bytes of slot storage per mmap growth step (page-rounded by [`OsMemory::map`]).
const CHUNK_BYTES: usize = 256 * 1024;
const MAX_CHUNKS: usize = 256;
const VACANT_END: u32 = u32::MAX;

/// Writer-owned mappings and bump. Readers never touch this.
struct Grow {
    mappings: [Option<Mapping>; MAX_CHUNKS],
    bump: u32,
}

pub(crate) struct Arena<T> {
    ptrs: [AtomicPtr<Slot<T>>; MAX_CHUNKS],
    /// One past the highest initialized index (Occupied or Vacant).
    len: AtomicU32,
    grow: Mutex<Grow>,
    vacant_head: u32,
    /// Opts out of auto `Send`/`Sync`: `get` yields `&T` from shared `&self`.
    marker: PhantomData<*const T>,
}

// SAFETY: reader atomics publish immovable slots; grow mutex serializes mapping
// ownership. Slots never move or unmap for the Arena lifetime.
unsafe impl<T: Send> Send for Arena<T> {}
// SAFETY: `get` / `push` share `&T` from a published Occupied slot, so T: Sync.
unsafe impl<T: Sync> Sync for Arena<T> {}

impl<T> Arena<T> {
    pub(crate) fn new() -> Self {
        Self {
            ptrs: core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            len: AtomicU32::new(0),
            grow: Mutex::new(Grow {
                mappings: core::array::from_fn(|_| None),
                bump: 0,
            }),
            vacant_head: VACANT_END,
            marker: PhantomData,
        }
    }

    fn slots_per_chunk() -> u32 {
        let slot = size_of::<Slot<T>>().max(1);
        debug_assert!(core::mem::align_of::<Slot<T>>() <= PAGE_SIZE);
        let n = (CHUNK_BYTES / slot).max(1);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn published(&self) -> u32 {
        self.len.load(Ordering::Acquire)
    }

    fn chunk_of(index: u32) -> Option<(usize, usize)> {
        let per = Self::slots_per_chunk();
        let chunk = usize::try_from(index / per).ok()?;
        let offset = usize::try_from(index % per).ok()?;
        (chunk < MAX_CHUNKS).then_some((chunk, offset))
    }

    fn slot_ptr(&self, index: u32) -> Option<NonNull<Slot<T>>> {
        if index >= self.published() {
            return None;
        }
        let (chunk_i, offset) = Self::chunk_of(index)?;
        let chunk = self.ptrs.get(chunk_i)?.load(Ordering::Acquire);
        if chunk.is_null() {
            return None;
        }
        // SAFETY: `len` was published after this slot was `ptr::write` and the
        // chunk pointer was stored. Occupied slots never move or unmap.
        Some(unsafe { NonNull::new_unchecked(chunk.add(offset)) })
    }

    fn slot(&self, index: u32) -> Option<&Slot<T>> {
        // SAFETY: `slot_ptr` yields a live published slot.
        Some(unsafe { self.slot_ptr(index)?.as_ref() })
    }

    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T>> {
        // SAFETY: exclusive `Arena` borrow; no concurrent `get`.
        Some(unsafe { self.slot_ptr(index)?.as_mut() })
    }

    /// Lock-free borrow of an Occupied slot.
    pub(crate) fn get(&self, index: u32) -> Option<&T> {
        self.slot(index)?.get()
    }

    pub(crate) fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        self.slot_mut(index)?.get_mut()
    }

    /// Initialize the next index. `init` sees the assigned index; `None` does
    /// not consume it. Maps a new chunk when the index is the first in that chunk.
    pub(crate) fn push(&self, init: impl FnOnce(u32) -> Option<T>) -> Option<(u32, &T)> {
        let mut grow = self.grow.lock();
        let index = grow.bump;
        if index == u32::MAX {
            return None;
        }
        let (chunk_i, offset) = Self::chunk_of(index)?;
        let base = if offset == 0 && grow.mappings.get(chunk_i).is_some_and(Option::is_none) {
            let byte_len = usize::try_from(Self::slots_per_chunk())
                .ok()?
                .checked_mul(size_of::<Slot<T>>())?;
            let mapping = OsMemory::map(byte_len)?;
            let base = mapping.base().cast::<Slot<T>>();
            *grow.mappings.get_mut(chunk_i)? = Some(mapping);
            self.ptrs
                .get(chunk_i)?
                .store(base.as_ptr(), Ordering::Release);
            base.as_ptr()
        } else {
            self.ptrs.get(chunk_i)?.load(Ordering::Relaxed)
        };
        if base.is_null() {
            return None;
        }
        // SAFETY: this offset is inside the mapped chunk and has never been
        // initialized (or `init` failed last time and bump did not advance).
        let slot = unsafe { NonNull::new_unchecked(base.add(offset)) };
        let value = init(index)?;
        // SAFETY: exclusive grow lock; slot is uninitialized mapped memory.
        unsafe { slot.as_ptr().write(Slot::Occupied(value)) };
        grow.bump = index + 1;
        self.len.store(grow.bump, Ordering::Release);
        // SAFETY: just written Occupied; published; grows only append.
        Some((index, unsafe {
            match slot.as_ref() {
                Slot::Occupied(value) => value,
                Slot::Vacant { .. } => core::hint::unreachable_unchecked(),
            }
        }))
    }

    /// Next vacant index. Does not occupy; a second call returns the same index
    /// until [`Self::insert`].
    pub(crate) fn vacant(&mut self) -> Option<u32> {
        if self.vacant_head != VACANT_END {
            return Some(self.vacant_head);
        }
        let next = self.published();
        if next == u32::MAX {
            return None;
        }
        let _ = Self::chunk_of(next)?;
        Some(next)
    }

    /// Occupy `index` from [`Self::vacant`]. Any other index is rejected.
    pub(crate) fn insert(&mut self, index: u32, value: T) -> Option<&mut T> {
        if self.vacant_head != VACANT_END {
            if index != self.vacant_head {
                return None;
            }
            let next = {
                let slot = self.slot_mut(index)?;
                let Slot::Vacant { next } = slot else {
                    return None;
                };
                let next = *next;
                *slot = Slot::Occupied(value);
                next
            };
            self.vacant_head = next;
            return self.get_mut(index);
        }

        if index != self.published() {
            return None;
        }
        let (index, _) = self.push(|_| Some(value))?;
        self.get_mut(index)
    }

    pub(crate) fn remove(&mut self, index: u32) -> Option<T> {
        let next = self.vacant_head;
        let slot = self.slot_mut(index)?;
        match core::mem::replace(slot, Slot::Vacant { next }) {
            Slot::Occupied(value) => {
                self.vacant_head = index;
                Some(value)
            }
            vacant @ Slot::Vacant { .. } => {
                *slot = vacant;
                None
            }
        }
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
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        let n = self.len.load(Ordering::Relaxed);
        for index in 0..n {
            let Some(slot) = self.slot_ptr(index) else {
                continue;
            };
            // SAFETY: exclusive Arena drop; each slot was `ptr::write` exactly once.
            unsafe { ptr::drop_in_place(slot.as_ptr()) };
        }
    }
}

pub(crate) struct Iter<'a, T> {
    arena: &'a Arena<T>,
    index: u32,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.arena.published() {
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
        while self.index < arena.published() {
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

impl<T> Slot<T> {
    const fn get(&self) -> Option<&T> {
        match self {
            Self::Occupied(value) => Some(value),
            Self::Vacant { .. } => None,
        }
    }

    const fn get_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Occupied(value) => Some(value),
            Self::Vacant { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use std::sync::mpsc;
    use std::thread;

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
        let per = Arena::<Large>::slots_per_chunk();
        let entries = per * 32 + 1;
        for index in 0..entries {
            let mut value = Large([0; 4096]);
            value.0[..4].copy_from_slice(&index.to_le_bytes());
            let slot = arena.vacant().unwrap();
            arena.insert(slot, value).unwrap();
        }

        assert_eq!(&arena.get(0).unwrap().0[..4], &0u32.to_le_bytes());
        assert_eq!(
            &arena.get(entries - 1).unwrap().0[..4],
            &(entries - 1).to_le_bytes()
        );
    }

    #[test]
    fn arena_grows_across_chunks_without_moving_slots() {
        #[repr(C)]
        struct Large([u8; 4096]);

        let mut arena = Arena::<Large>::new();
        let first_index = arena.vacant().unwrap();
        arena.insert(first_index, Large([0; 4096])).unwrap();
        let first = NonNull::from(arena.get(0).unwrap());
        let per = Arena::<Large>::slots_per_chunk();
        let entries = per + 1;

        for index in 1..entries {
            let mut value = Large([0; 4096]);
            value.0[..4].copy_from_slice(&index.to_le_bytes());
            let slot = arena.vacant().unwrap();
            arena.insert(slot, value).unwrap();
        }

        assert_eq!(NonNull::from(arena.get(0).unwrap()), first);
        assert_eq!(arena.iter().count(), usize::try_from(entries).unwrap());
        assert_eq!(
            &arena.get(entries - 1).unwrap().0[..4],
            &(entries - 1).to_le_bytes()
        );
    }

    #[test]
    fn get_sees_published_slots_across_chunks() {
        let arena = Arena::<u32>::new();
        let n = Arena::<u32>::slots_per_chunk() + 8;
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            let arena = &arena;
            scope.spawn(move || {
                for i in 0..n {
                    let (index, _) = arena.push(|_| Some(i)).unwrap();
                    tx.send(index).unwrap();
                }
            });
            let mut seen = 0u32;
            for index in rx {
                while arena.get(index).is_none() {
                    core::hint::spin_loop();
                }
                assert_eq!(*arena.get(index).unwrap(), index);
                seen += 1;
            }
            assert_eq!(seen, n);
        });
    }
}
