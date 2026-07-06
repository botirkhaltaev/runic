use core::ptr::NonNull;

use crate::{
    allocation::{Allocation, ZeroStatus},
    layout::LayoutSpec,
    memory::{PageEntry, PageMap, PageRange},
    run::{RUN_SIZE, Run, RunError, RunId, RunTable},
    size_class::{SizeClass, SizeClasses},
};

use super::RunReservation;

pub(crate) struct RunAllocator {
    runs: RunTable,
    active: [Option<RunId>; SizeClasses::COUNT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunAllocatorError {
    MissingRun,
    InvalidPointer,
    DoubleFree,
    InvalidMetadata,
}

impl RunAllocator {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            runs: RunTable::new(capacity),
            active: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        class: SizeClass,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let class_index = class.id().index();
        let active_id = self.active.get(class_index).copied().flatten();

        if let Some(id) = active_id
            && let Some(run) = self.runs.get_mut(id)
            && let Some(ptr) = run.allocate(spec)
        {
            return Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing));
        }

        if let Some((id, ptr)) = self.runs.allocate(class, spec) {
            let active_slot = self.active.get_mut(class_index)?;
            *active_slot = Some(id);

            return Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing));
        }

        let mapping = crate::memory::OsMemory::map(RUN_SIZE)?;
        let reservation = self.runs.reserve()?;
        let id = reservation.id();

        let run = Run::new(id, mapping, class);
        if self.insert_run(reservation, run, pages).is_err() {
            return None;
        }

        let active_slot = self.active.get_mut(class_index)?;
        *active_slot = Some(id);

        let inserted_run = self.runs.get_mut(id)?;
        inserted_run
            .allocate(spec)
            .map(|ptr| Allocation::new(ptr, ZeroStatus::NeedsZeroing))
    }

    pub(crate) fn free(&mut self, id: RunId, ptr: NonNull<u8>) -> Result<(), RunAllocatorError> {
        let class_index = {
            let Some(run) = self.runs.get_mut(id) else {
                return Err(RunAllocatorError::MissingRun);
            };

            run.free(ptr).map_err(RunAllocatorError::from)?;

            run.class().index()
        };

        let Some(active_slot) = self.active.get_mut(class_index) else {
            return Err(RunAllocatorError::InvalidMetadata);
        };
        *active_slot = Some(id);

        Ok(())
    }

    pub(crate) fn validate_allocated(
        &self,
        id: RunId,
        ptr: NonNull<u8>,
    ) -> Result<(), RunAllocatorError> {
        let Some(run) = self.runs.get(id) else {
            return Err(RunAllocatorError::MissingRun);
        };

        run.allocated_block_at(ptr)
            .map_err(RunAllocatorError::from)?;

        Ok(())
    }

    fn insert_run(
        &mut self,
        reservation: RunReservation,
        run: Run,
        pages: &mut PageMap,
    ) -> Result<RunId, ()> {
        let id = reservation.id();
        let range = run.range();

        if self.runs.insert(reservation, run).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            let _removed = self.runs.remove(id);
            return Err(());
        };

        if pages.insert(page_range, PageEntry::Run(id)).is_err() {
            let _removed = self.runs.remove(id);
            return Err(());
        }

        Ok(id)
    }
}

impl From<RunError> for RunAllocatorError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidPointer => Self::InvalidPointer,
            RunError::DoubleFree => Self::DoubleFree,
            RunError::FreeUnderflow => Self::InvalidMetadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        layout::LayoutSpec,
        memory::{OsMemory, PageEntry, PageMap, PageRange},
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

    #[test]
    fn failed_run_page_publication_removes_table_entry() {
        let mut allocator = RunAllocator::new(4);
        let mut pages = PageMap::new();
        let reservation = allocator.runs.reserve().unwrap();
        let id = reservation.id();
        let run = reusable_run(id);
        let range = run.range();
        let page_range = PageRange::new(range.base(), range.len()).unwrap();
        let existing = PageEntry::Run(RunId::from_index(900).unwrap());

        pages.insert(page_range, existing).unwrap();

        assert_eq!(allocator.insert_run(reservation, run, &mut pages), Err(()));
        assert!(allocator.runs.get(id).is_none());
        assert_eq!(pages.get(range.base()), Some(existing));
    }
}
