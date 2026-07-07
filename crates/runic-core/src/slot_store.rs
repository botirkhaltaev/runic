use core::{mem::MaybeUninit, ptr::NonNull, slice};

use crate::memory::{Mapping, OsMemory};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlotStoreError {
    InvalidIndex,
    NotReserved,
    Occupied,
}

pub(crate) struct SlotStore<T> {
    slots: Option<Slots<T>>,
    next: u32,
    capacity: u32,
}

impl<T> SlotStore<T> {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            slots: None,
            next: 0,
            capacity,
        }
    }

    pub(crate) fn reserve(&mut self) -> Option<usize> {
        let capacity = self.capacity()?;
        if capacity == 0 {
            return None;
        }
        let start = usize::try_from(self.next).ok()?;

        for offset in 0..capacity {
            let sum = start.checked_add(offset)?;
            let index = if sum >= capacity {
                sum.checked_sub(capacity)?
            } else {
                sum
            };

            let slot = self.slots_mut()?.get_mut(index)?;
            if slot.reserve() {
                let next = if index + 1 == capacity {
                    0
                } else {
                    index.checked_add(1)?
                };
                self.next = u32::try_from(next).ok()?;

                return Some(index);
            }
        }

        None
    }

    pub(crate) fn release(&mut self, index: usize) -> bool {
        self.slot_mut(index).is_some_and(Slot::release)
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) -> Result<(), SlotStoreError> {
        self.slot_mut(index)
            .ok_or(SlotStoreError::InvalidIndex)?
            .insert(value)
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slot_mut(index)?.get_mut()
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<T> {
        self.slot_mut(index)?.remove()
    }

    pub(crate) fn capacity(&self) -> Option<usize> {
        usize::try_from(self.capacity).ok()
    }

    fn slots_mut(&mut self) -> Option<&mut Slots<T>> {
        if self.slots.is_none() {
            self.slots = Some(Slots::new(self.capacity()?)?);
        }

        self.slots.as_mut()
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut Slot<T>> {
        self.slots.as_mut()?.get_mut(index)
    }
}

struct Slots<T> {
    mapping: Mapping,
    base: NonNull<Slot<T>>,
    len: usize,
}

// SAFETY: Slots owns mmap-backed storage. Moving ownership to another
// thread does not permit concurrent mutation of allocator metadata.
unsafe impl<T: Send> Send for Slots<T> {}

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

    fn slots_mut(&mut self) -> &mut [Slot<T>] {
        debug_assert!(self.len <= self.mapping.range().len() / core::mem::size_of::<Slot<T>>());

        // SAFETY: Slots has unique access to the mmap storage here.
        unsafe { slice::from_raw_parts_mut(self.base.as_ptr(), self.len) }
    }
}

impl<T> Drop for Slots<T> {
    fn drop(&mut self) {
        for slot in self.slots_mut() {
            slot.drop_value();
        }
    }
}

#[repr(C)]
struct Slot<T> {
    value: MaybeUninit<T>,
    state: SlotState,
}

impl<T> Slot<T> {
    fn reserve(&mut self) -> bool {
        if !self.state.is_empty() {
            return false;
        }

        self.state = SlotState::reserved();
        true
    }

    fn release(&mut self) -> bool {
        if !self.state.is_reserved() {
            return false;
        }

        self.state = SlotState::empty();
        true
    }

    fn insert(&mut self, value: T) -> Result<(), SlotStoreError> {
        if self.state.is_occupied() {
            return Err(SlotStoreError::Occupied);
        }
        if !self.state.is_reserved() {
            return Err(SlotStoreError::NotReserved);
        }

        self.value.write(value);
        self.state = SlotState::occupied();

        Ok(())
    }

    fn get_mut(&mut self) -> Option<&mut T> {
        if !self.state.is_occupied() {
            return None;
        }

        // SAFETY: occupied state is set only after value.write initializes the slot.
        Some(unsafe { self.value.assume_init_mut() })
    }

    fn remove(&mut self) -> Option<T> {
        if !self.state.is_occupied() {
            return None;
        }

        self.state = SlotState::empty();

        // SAFETY: occupied state was true on entry, so the slot contains an initialized T.
        Some(unsafe { self.value.assume_init_read() })
    }

    fn drop_value(&mut self) {
        let _ = self.remove();
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct SlotState {
    raw: u8,
}

impl SlotState {
    const EMPTY: u8 = 0;
    const RESERVED: u8 = 1;
    const OCCUPIED: u8 = 2;

    const fn empty() -> Self {
        Self { raw: Self::EMPTY }
    }

    const fn reserved() -> Self {
        Self {
            raw: Self::RESERVED,
        }
    }

    const fn occupied() -> Self {
        Self {
            raw: Self::OCCUPIED,
        }
    }

    const fn is_empty(self) -> bool {
        self.raw == Self::EMPTY
    }

    const fn is_reserved(self) -> bool {
        self.raw == Self::RESERVED
    }

    const fn is_occupied(self) -> bool {
        self.raw == Self::OCCUPIED
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
    fn slot_store_zero_capacity_reserves_none() {
        let mut store = SlotStore::<u32>::new(0);

        assert_eq!(store.reserve(), None);
    }

    #[test]
    fn slot_store_reserves_indices_from_zero() {
        let mut store = SlotStore::<u32>::new(4);

        assert_eq!(store.reserve(), Some(0));
        assert_eq!(store.reserve(), Some(1));
    }

    #[test]
    fn slot_store_respects_capacity() {
        let mut store = SlotStore::<u32>::new(2);

        assert_eq!(store.reserve(), Some(0));
        assert_eq!(store.reserve(), Some(1));
        assert_eq!(store.reserve(), None);
    }

    #[test]
    fn slot_store_release_makes_reserved_slot_available() {
        let mut store = SlotStore::<u32>::new(4);
        let first = store.reserve().unwrap();
        let second = store.reserve().unwrap();

        assert!(store.release(first));

        assert_eq!(second, 1);
        for expected in 2..4 {
            assert_eq!(store.reserve(), Some(expected));
        }
        assert_eq!(store.reserve(), Some(first));
    }

    #[test]
    fn slot_store_insert_get_remove_round_trip() {
        let mut store = SlotStore::<u32>::new(4);
        let index = store.reserve().unwrap();

        assert_eq!(store.insert(index, 42), Ok(()));
        assert_eq!(store.get_mut(index).copied(), Some(42));
        assert_eq!(store.remove(index), Some(42));
        assert_eq!(store.get_mut(index), None);
    }

    #[test]
    fn slot_store_get_mut_allows_mutation() {
        let mut store = SlotStore::<u32>::new(4);
        let index = store.reserve().unwrap();
        assert_eq!(store.insert(index, 42), Ok(()));

        *store.get_mut(index).unwrap() = 7;

        assert_eq!(store.get_mut(index).copied(), Some(7));
    }

    #[test]
    fn slot_store_rejects_occupied_insert() {
        let mut store = SlotStore::<u32>::new(4);
        let index = store.reserve().unwrap();
        assert_eq!(store.insert(index, 1), Ok(()));

        assert_eq!(store.insert(index, 2), Err(SlotStoreError::Occupied));
        assert_eq!(store.get_mut(index).copied(), Some(1));
    }

    #[test]
    fn slot_store_rejects_unreserved_insert() {
        let mut store = SlotStore::<u32>::new(4);
        assert_eq!(store.reserve(), Some(0));

        assert_eq!(store.insert(1, 2), Err(SlotStoreError::NotReserved));
        assert_eq!(store.get_mut(1), None);
    }

    #[test]
    fn slot_store_rejects_invalid_index() {
        let mut store = SlotStore::<u32>::new(1);
        assert_eq!(store.reserve(), Some(0));

        assert_eq!(store.insert(1, 2), Err(SlotStoreError::InvalidIndex));
        assert!(!store.release(1));
    }

    #[test]
    fn slot_store_drop_removes_only_occupied_values() {
        let drops = Cell::new(0);
        {
            let mut store = SlotStore::new(4);
            let occupied = store.reserve().unwrap();
            let reserved = store.reserve().unwrap();

            assert_eq!(
                store.insert(occupied, DropCounter { drops: &drops }),
                Ok(())
            );
            assert_ne!(occupied, reserved);
        }

        assert_eq!(drops.get(), 1);
    }
}
