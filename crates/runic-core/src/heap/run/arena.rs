use crate::heap::{Run, RunId};

use super::super::arena::Arena;

pub(crate) type RunArena = Arena<Run, RunId>;

#[cfg(test)]
mod tests {
    use crate::{
        heap::{Owner, RUN_SIZE, Run, RunId},
        layout::LayoutSpec,
        memory::OsMemory,
        size_class::SizeClasses,
    };

    use super::super::super::arena::{ArenaError, ArenaReservation};

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();

        Run::new(id, Owner::Central, mapping, class)
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
        assert_eq!(arena.reserve().unwrap().id.index(), 0);
        assert_eq!(arena.reserve().unwrap().id.index(), 1);
    }

    #[test]
    fn run_arena_respects_injected_capacity() {
        let mut arena = arena_with_capacity(2);
        assert_eq!(arena.reserve().unwrap().id.index(), 0);
        assert_eq!(arena.reserve().unwrap().id.index(), 1);
        assert_eq!(arena.reserve(), None);
    }

    #[test]
    fn run_arena_release_makes_reserved_slot_available() {
        let mut arena = arena_with_capacity(4);
        let first = arena.reserve().unwrap();
        let second = arena.reserve().unwrap();

        arena.release(first);

        assert_eq!(second.id.index(), 1);
        for expected in 2..4 {
            assert_eq!(arena.reserve().unwrap().id.index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id, first.id);
    }

    #[test]
    fn run_arena_insert_get_round_trip() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let run = reusable_run(reservation.id);
        let id = arena.insert(reservation, run).unwrap();
        assert_eq!(arena.get_mut(id).unwrap().id, id);

        let run = arena.remove(id).unwrap();
        assert_eq!(run.id, id);
    }

    #[test]
    fn run_arena_rejects_occupied_slot() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let first = reusable_run(reservation.id);
        let second = reusable_run(reservation.id);

        let id = arena.insert(reservation, first).unwrap();
        assert_eq!(
            arena.insert(ArenaReservation { id }, second),
            Err(ArenaError::Occupied)
        );

        let _removed = arena.remove(id);
    }

    #[test]
    fn run_arena_rejects_unreserved_insert() {
        let mut arena = arena_with_capacity(4);
        let id = RunId::from_index(0).unwrap();
        let run = reusable_run(id);
        assert_eq!(
            arena.insert(ArenaReservation { id }, run),
            Err(ArenaError::InvalidReservation)
        );
    }

    #[test]
    fn run_arena_invalid_insert_releases_reservation() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let reserved_id = reservation.id;
        // Create a value with a DIFFERENT id than the reservation
        let wrong_id = RunId::from_index(reserved_id.index() + 1).unwrap();
        let wrong_run = reusable_run(wrong_id);
        // This insert should fail because reservation.id != value.id()
        assert_eq!(
            arena.insert(reservation, wrong_run),
            Err(ArenaError::InvalidReservation)
        );

        // The reservation should have been released, so we can reserve the same slot again
        for expected in 1..4 {
            assert_eq!(arena.reserve().unwrap().id.index(), expected);
        }
        assert_eq!(arena.reserve().unwrap().id, reserved_id);
    }

    #[test]
    fn run_arena_get_mut_allows_run_mutation() {
        let mut arena = arena_with_capacity(4);
        let reservation = arena.reserve().unwrap();
        let run = reusable_run(reservation.id);
        let id = arena.insert(reservation, run).unwrap();
        let ptr = arena.get_mut(id).unwrap().allocate().unwrap();

        assert!(arena.get_mut(id).unwrap().free_local(ptr).is_ok());

        let _removed = arena.remove(id);
    }
}
