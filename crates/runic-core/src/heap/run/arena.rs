use crate::{
    run::{Run, RunId},
    slot_store::{SlotStore, SlotStoreError},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunArenaError {
    InvalidReservation,
    Occupied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunReservation {
    id: RunId,
}

impl RunReservation {
    pub(crate) const fn id(self) -> RunId {
        self.id
    }
}

pub(crate) struct RunArena {
    slots: SlotStore<Run>,
}

// SAFETY: RunArena owns allocator metadata accessed through the global heap lock.
// Moving ownership to another thread does not permit concurrent metadata mutation.
unsafe impl Send for RunArena {}

impl RunArena {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            slots: SlotStore::new(capacity),
        }
    }

    pub(crate) fn reserve(&mut self) -> Option<RunReservation> {
        let index = self.slots.reserve()?;
        let Some(id) = Self::id(index) else {
            let _ = self.slots.release(index);
            return None;
        };

        Some(RunReservation { id })
    }

    pub(crate) fn release(&mut self, reservation: RunReservation) {
        let Some(index) = Self::index(reservation.id) else {
            return;
        };

        let _ = self.slots.release(index);
    }

    pub(crate) fn insert(
        &mut self,
        reservation: RunReservation,
        run: Run,
    ) -> Result<RunId, RunArenaError> {
        if reservation.id != run.id() {
            self.release(reservation);
            return Err(RunArenaError::InvalidReservation);
        }

        let Some(index) = Self::index(reservation.id) else {
            return Err(RunArenaError::InvalidReservation);
        };

        self.slots.insert(index, run).map_err(RunArenaError::from)?;

        Ok(reservation.id)
    }

    pub(crate) fn get_mut(&mut self, id: RunId) -> Option<&mut Run> {
        self.slots.get_mut(Self::index(id)?)
    }

    pub(crate) fn remove(&mut self, id: RunId) -> Option<Run> {
        self.slots.remove(Self::index(id)?)
    }

    fn index(id: RunId) -> Option<usize> {
        usize::try_from(id.index()).ok()
    }

    fn id(index: usize) -> Option<RunId> {
        RunId::from_index(u32::try_from(index).ok()?)
    }
}

impl From<SlotStoreError> for RunArenaError {
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
        layout::LayoutSpec,
        memory::OsMemory,
        ownership::HeapOwner,
        run::{RUN_SIZE, Run, RunId},
        size_class::SizeClasses,
    };

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();

        Run::new(id, HeapOwner::Shared, mapping, class)
    }

    fn arena_with_capacity(capacity: usize) -> RunArena {
        RunArena::new(u32::try_from(capacity).unwrap())
    }

    #[test]
    fn run_arena_zero_capacity_reserves_none() {
        let mut arena = RunArena::new(0);

        assert_eq!(arena.reserve(), None);
    }

    #[test]
    fn run_arena_reserves_ids_from_zero() {
        let mut arena = arena_with_capacity(4);

        assert_eq!(arena.reserve().unwrap().id().index(), 0);
        assert_eq!(arena.reserve().unwrap().id().index(), 1);
    }

    #[test]
    fn run_arena_respects_injected_capacity() {
        let mut arena = arena_with_capacity(2);

        assert_eq!(arena.reserve().unwrap().id().index(), 0);
        assert_eq!(arena.reserve().unwrap().id().index(), 1);
        assert_eq!(arena.reserve(), None);
    }

    #[test]
    fn run_arena_release_makes_reserved_slot_available() {
        let mut arena = arena_with_capacity(4);
        let first = arena.reserve().unwrap();
        let second = arena.reserve().unwrap();

        arena.release(first);

        assert_eq!(second.id().index(), 1);
        for expected in 2..4 {
            assert_eq!(arena.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id(), first.id());
    }

    #[test]
    fn run_arena_insert_get_round_trip() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let run = reusable_run(reservation.id());

        let id = arena.insert(reservation, run).unwrap();
        assert_eq!(arena.get_mut(id).unwrap().id(), id);

        let run = arena.remove(id).unwrap();
        assert_eq!(run.id(), id);
    }

    #[test]
    fn run_arena_rejects_occupied_slot() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let first = reusable_run(reservation.id());
        let second = reusable_run(reservation.id());

        let id = arena.insert(reservation, first).unwrap();
        assert_eq!(
            arena.insert(RunReservation { id }, second),
            Err(RunArenaError::Occupied)
        );

        let _removed = arena.remove(id);
    }

    #[test]
    fn run_arena_rejects_unreserved_insert() {
        let mut arena = arena_with_capacity(4);
        let id = RunId::from_index(0).unwrap();
        let run = reusable_run(id);

        assert_eq!(
            arena.insert(RunReservation { id }, run),
            Err(RunArenaError::InvalidReservation)
        );
    }

    #[test]
    fn run_arena_invalid_insert_releases_reservation() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let released = reservation.id();
        let wrong_id = RunId::from_index(released.index() + 1).unwrap();
        let run = reusable_run(wrong_id);

        assert_eq!(
            arena.insert(reservation, run),
            Err(RunArenaError::InvalidReservation)
        );

        for expected in 1..4 {
            assert_eq!(arena.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id(), released);
    }

    #[test]
    fn run_arena_get_mut_allows_run_mutation() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let run = reusable_run(reservation.id());

        let id = arena.insert(reservation, run).unwrap();
        let ptr = arena.get_mut(id).unwrap().allocate().unwrap().ptr();

        assert!(arena.get_mut(id).unwrap().free(ptr).is_ok());

        let _removed = arena.remove(id);
    }

    #[test]
    fn run_arena_remove_clears_slot() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let run = reusable_run(reservation.id());

        let id = arena.insert(reservation, run).unwrap();
        assert!(arena.remove(id).is_some());
        assert!(arena.get_mut(id).is_none());
    }
}
