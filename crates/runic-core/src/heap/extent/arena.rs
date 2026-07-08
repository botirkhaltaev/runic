use crate::{
    extent::{Extent, ExtentId},
    slot_store::{SlotStore, SlotStoreError},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentArenaError {
    InvalidReservation,
    Occupied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentReservation {
    id: ExtentId,
}

impl ExtentReservation {
    pub(crate) const fn id(self) -> ExtentId {
        self.id
    }
}

pub(crate) struct ExtentArena {
    slots: SlotStore<Extent>,
}

// SAFETY: ExtentArena owns allocator metadata accessed through the global heap lock.
// Moving ownership to another thread does not permit concurrent metadata mutation.
unsafe impl Send for ExtentArena {}

impl ExtentArena {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            slots: SlotStore::new(capacity),
        }
    }

    pub(crate) fn reserve(&mut self) -> Option<ExtentReservation> {
        let index = self.slots.reserve()?;
        let Some(id) = Self::id(index) else {
            let _ = self.slots.release(index);
            return None;
        };

        Some(ExtentReservation { id })
    }

    pub(crate) fn release(&mut self, reservation: ExtentReservation) {
        let Some(index) = Self::index(reservation.id) else {
            return;
        };

        let _ = self.slots.release(index);
    }

    pub(crate) fn insert(
        &mut self,
        reservation: ExtentReservation,
        extent: Extent,
    ) -> Result<ExtentId, ExtentArenaError> {
        if reservation.id != extent.id() {
            self.release(reservation);
            return Err(ExtentArenaError::InvalidReservation);
        }

        let Some(index) = Self::index(reservation.id) else {
            return Err(ExtentArenaError::InvalidReservation);
        };

        self.slots
            .insert(index, extent)
            .map_err(ExtentArenaError::from)?;

        Ok(reservation.id)
    }

    pub(crate) fn get_mut(&mut self, id: ExtentId) -> Option<&mut Extent> {
        self.slots.get_mut(Self::index(id)?)
    }

    pub(crate) fn remove(&mut self, id: ExtentId) -> Option<Extent> {
        self.slots.remove(Self::index(id)?)
    }

    fn index(id: ExtentId) -> Option<usize> {
        usize::try_from(id.index()).ok()
    }

    fn id(index: usize) -> Option<ExtentId> {
        ExtentId::from_index(u32::try_from(index).ok()?)
    }
}

impl From<SlotStoreError> for ExtentArenaError {
    fn from(error: SlotStoreError) -> Self {
        match error {
            SlotStoreError::InvalidIndex | SlotStoreError::NotReserved => Self::InvalidReservation,
            SlotStoreError::Occupied => Self::Occupied,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        extent::{Extent, ExtentId},
        layout::LayoutSpec,
        memory::OsMemory,
        ownership::HeapOwner,
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, HeapOwner::Shared, mapping, spec).unwrap()
    }

    fn arena_with_capacity(capacity: usize) -> ExtentArena {
        ExtentArena::new(u32::try_from(capacity).unwrap())
    }

    #[test]
    fn extent_arena_zero_capacity_reserves_none() {
        let mut arena = ExtentArena::new(0);

        assert_eq!(arena.reserve(), None);
    }

    #[test]
    fn extent_arena_respects_injected_capacity() {
        let mut arena = arena_with_capacity(2);

        assert_eq!(arena.reserve().unwrap().id().index(), 0);
        assert_eq!(arena.reserve().unwrap().id().index(), 1);
        assert_eq!(arena.reserve(), None);
    }

    #[test]
    fn extent_arena_insert_get_round_trip() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let extent = reusable_extent(reservation.id());

        let id = arena.insert(reservation, extent).unwrap();
        assert_eq!(arena.get_mut(id).unwrap().id(), id);

        let extent = arena.remove(id).unwrap();
        assert_eq!(extent.id(), id);
    }

    #[test]
    fn extent_arena_invalid_insert_releases_reservation() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let released = reservation.id();
        let wrong_id = ExtentId::from_index(released.index() + 1).unwrap();
        let extent = reusable_extent(wrong_id);

        assert_eq!(
            arena.insert(reservation, extent),
            Err(ExtentArenaError::InvalidReservation)
        );

        for expected in 1..4 {
            assert_eq!(arena.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id(), released);
    }
}
