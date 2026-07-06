use core::ptr::NonNull;

use crate::{
    layout::LayoutSpec,
    run::{Run, RunId},
    size_class::SizeClass,
    slot_store::{SlotStore, SlotStoreError},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunTableError {
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

pub(crate) struct RunTable {
    slots: SlotStore<Run>,
}

// SAFETY: RunTable owns allocator metadata accessed through the global heap lock.
// Moving ownership to another thread does not permit concurrent metadata mutation.
unsafe impl Send for RunTable {}

impl RunTable {
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
    ) -> Result<RunId, RunTableError> {
        if reservation.id != run.id() {
            self.release(reservation);
            return Err(RunTableError::InvalidReservation);
        }

        let Some(index) = Self::index(reservation.id) else {
            return Err(RunTableError::InvalidReservation);
        };

        self.slots.insert(index, run).map_err(RunTableError::from)?;

        Ok(reservation.id)
    }

    pub(crate) fn get(&self, id: RunId) -> Option<&Run> {
        self.slots.get(Self::index(id)?)
    }

    pub(crate) fn get_mut(&mut self, id: RunId) -> Option<&mut Run> {
        self.slots.get_mut(Self::index(id)?)
    }

    pub(crate) fn allocate(
        &mut self,
        class: SizeClass,
        spec: LayoutSpec,
    ) -> Option<(RunId, NonNull<u8>)> {
        for (index, run) in self.slots.occupied_mut()? {
            if run.class() != class.id() {
                continue;
            }

            let Some(ptr) = run.allocate(spec) else {
                continue;
            };
            let id = Self::id(index)?;

            return Some((id, ptr));
        }

        None
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

impl From<SlotStoreError> for RunTableError {
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
        run::{RUN_SIZE, Run, RunId},
        size_class::SizeClasses,
    };

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::get(spec).unwrap();

        Run::new(id, mapping, class)
    }

    fn table_with_capacity(capacity: usize) -> RunTable {
        RunTable::new(u32::try_from(capacity).unwrap())
    }

    #[test]
    fn run_table_zero_capacity_reserves_none() {
        let mut table = RunTable::new(0);

        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn run_table_reserves_ids_from_zero() {
        let mut table = table_with_capacity(4);

        assert_eq!(table.reserve().unwrap().id().index(), 0);
        assert_eq!(table.reserve().unwrap().id().index(), 1);
    }

    #[test]
    fn run_table_respects_injected_capacity() {
        let mut table = table_with_capacity(2);

        assert_eq!(table.reserve().unwrap().id().index(), 0);
        assert_eq!(table.reserve().unwrap().id().index(), 1);
        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn run_table_release_makes_reserved_slot_available() {
        let mut table = table_with_capacity(4);
        let first = table.reserve().unwrap();
        let second = table.reserve().unwrap();

        table.release(first);

        assert_eq!(second.id().index(), 1);
        for expected in 2..4 {
            assert_eq!(table.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(table.reserve().unwrap().id(), first.id());
    }

    #[test]
    fn run_table_insert_get_round_trip() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let run = reusable_run(reservation.id());

        let id = table.insert(reservation, run).unwrap();
        assert_eq!(table.get(id).unwrap().id(), id);

        let run = table.remove(id).unwrap();
        assert_eq!(run.id(), id);
    }

    #[test]
    fn run_table_rejects_occupied_slot() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let first = reusable_run(reservation.id());
        let second = reusable_run(reservation.id());

        let id = table.insert(reservation, first).unwrap();
        assert_eq!(
            table.insert(RunReservation { id }, second),
            Err(RunTableError::Occupied)
        );

        let _removed = table.remove(id);
    }

    #[test]
    fn run_table_rejects_unreserved_insert() {
        let mut table = table_with_capacity(4);
        let id = RunId::from_index(0).unwrap();
        let run = reusable_run(id);

        assert_eq!(
            table.insert(RunReservation { id }, run),
            Err(RunTableError::InvalidReservation)
        );
    }

    #[test]
    fn run_table_invalid_insert_releases_reservation() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let released = reservation.id();
        let wrong_id = RunId::from_index(released.index() + 1).unwrap();
        let run = reusable_run(wrong_id);

        assert_eq!(
            table.insert(reservation, run),
            Err(RunTableError::InvalidReservation)
        );

        for expected in 1..4 {
            assert_eq!(table.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(table.reserve().unwrap().id(), released);
    }

    #[test]
    fn run_table_get_mut_allows_run_mutation() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let run = reusable_run(reservation.id());
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();

        let id = table.insert(reservation, run).unwrap();
        let ptr = table.get_mut(id).unwrap().allocate(spec).unwrap();

        assert!(table.get_mut(id).unwrap().free(ptr).is_ok());

        let _removed = table.remove(id);
    }

    #[test]
    fn run_table_remove_clears_slot() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let run = reusable_run(reservation.id());

        let id = table.insert(reservation, run).unwrap();
        assert!(table.remove(id).is_some());
        assert!(table.get(id).is_none());
        assert!(table.get_mut(id).is_none());
    }
}
