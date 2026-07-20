//! Fixed-capacity mmap object table with an intrusive freelist.
//!
//! Slots are empty or occupied only. `claim` checks an index out of the freelist;
//! callers that fail before `insert` must `release` it.

use core::{mem::MaybeUninit, ptr::NonNull, slice};

use crate::memory::{Mapping, OsMemory};

const FREE_END: u32 = u32::MAX;

pub(crate) struct Arena<T> {
    slots: Option<Slots<T>>,
    free_head: u32,
}

// SAFETY: Arena owns mmap-backed storage. Moving ownership does not permit concurrent mutation.
unsafe impl<T: Send> Send for Arena<T> {}

impl<T> Arena<T> {
    pub(crate) fn new(capacity: u32) -> Self {
        if capacity == 0 {
            return Self {
                slots: None,
                free_head: FREE_END,
            };
        }

        let Some(len) = usize::try_from(capacity).ok() else {
            return Self {
                slots: None,
                free_head: FREE_END,
            };
        };

        let Some(mut slots) = Slots::new(len) else {
            return Self {
                slots: None,
                free_head: FREE_END,
            };
        };

        // Link every slot into the freelist: 0 -> 1 -> ... -> END.
        for index in 0..len {
            let next = if index + 1 == len {
                FREE_END
            } else {
                u32::try_from(index + 1).unwrap_or(FREE_END)
            };
            slots.slot_mut(index).set_empty(next);
        }

        Self {
            slots: Some(slots),
            free_head: 0,
        }
    }

    pub(crate) fn claim(&mut self) -> Option<usize> {
        if self.free_head == FREE_END {
            return None;
        }

        let index = usize::try_from(self.free_head).ok()?;
        let slot = self.slots.as_mut()?.slot_mut(index);
        debug_assert!(!slot.is_occupied());
        self.free_head = slot.take_next();
        Some(index)
    }

    pub(crate) fn release(&mut self, index: usize) {
        let Some(slots) = self.slots.as_mut() else {
            return;
        };
        let Some(slot) = slots.get_mut(index) else {
            return;
        };
        if slot.is_occupied() {
            return;
        }

        slot.set_empty(self.free_head);
        let Ok(head) = u32::try_from(index) else {
            return;
        };
        self.free_head = head;
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) -> Option<&mut T> {
        let slot = self.slots.as_mut()?.get_mut(index)?;
        if slot.is_occupied() {
            return None;
        }

        slot.occupy(value);
        slot.get_mut()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.slots.as_ref()?.get(index)?.get()
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slots.as_mut()?.get_mut(index)?.get_mut()
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<T> {
        let value = self.slots.as_mut()?.get_mut(index)?.remove()?;
        self.release(index);
        Some(value)
    }
}

struct Slots<T> {
    /// Owns the mmap; dropped after slot values are cleared below.
    mapping: Mapping,
    base: NonNull<Slot<T>>,
    len: usize,
}

impl<T> Drop for Slots<T> {
    fn drop(&mut self) {
        for slot in self.slots_mut() {
            slot.drop_value();
        }
    }
}

impl<T> Slots<T> {
    fn new(len: usize) -> Option<Self> {
        let byte_len = len.checked_mul(core::mem::size_of::<Slot<T>>())?;
        let mapping = OsMemory::map(byte_len)?;
        let base = mapping.base().cast::<Slot<T>>();

        Some(Self { mapping, base, len })
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut Slot<T>> {
        self.slots_mut().get_mut(index)
    }

    fn get(&self, index: usize) -> Option<&Slot<T>> {
        self.slots().get(index)
    }

    fn slot_mut(&mut self, index: usize) -> &mut Slot<T> {
        // SAFETY: callers only use indices within len after freelist init.
        unsafe { self.slots_mut().get_unchecked_mut(index) }
    }

    fn slots_mut(&mut self) -> &mut [Slot<T>] {
        debug_assert!(self.len <= self.mapping.range().len() / core::mem::size_of::<Slot<T>>());

        // SAFETY: unique access to mmap storage sized for `len` slots.
        unsafe { slice::from_raw_parts_mut(self.base.as_ptr(), self.len) }
    }

    fn slots(&self) -> &[Slot<T>] {
        debug_assert!(self.len <= self.mapping.range().len() / core::mem::size_of::<Slot<T>>());

        // SAFETY: shared access to mmap storage sized for `len` slots.
        unsafe { slice::from_raw_parts(self.base.as_ptr(), self.len) }
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
}
