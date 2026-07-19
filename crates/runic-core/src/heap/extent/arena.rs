use crate::{
    heap::{Extent, ExtentId},
    slot_store::SlotStoreError,
};

use super::arena::{Arena, ArenaError, ArenaId, ArenaReservation};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentArenaError {
    InvalidReservation,
    Occupied,
}

impl From<ArenaError> for ExtentArenaError {
    fn from(error: ArenaError) -> Self {
        match error {
            ArenaError::InvalidReservation => Self::InvalidReservation,
            ArenaError::Occupied => Self::Occupied,
        }
    }
}

impl ArenaId for ExtentId {
    fn index(self) -> u32 {
        self.index()
    }

    fn from_index(index: u32) -> Option<Self> {
        ExtentId::from_index(index)
    }
}

pub(crate) type ExtentArena = Arena<Extent, ExtentId>;
pub(crate) type ExtentReservation = ArenaReservation<ExtentId>;

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
        heap::{Extent, ExtentId, HeapId},
        layout::LayoutSpec,
        memory::OsMemory,
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, Owner::for_heap(HeapId::ROOT), mapping, spec).unwrap()
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
        let wrong_id = ExtentReservation {
            id: ExtentId::from_index(released.index() + 1).unwrap(),
        };
        let extent = reusable_extent(wrong_id);

        assert_eq!(
            arena.insert(wrong_id, extent),
            Err(ExtentArenaError::InvalidReservation)
        );

        for expected in 1..4 {
            assert_eq!(arena.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id(), released);
    }
}