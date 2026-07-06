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
    available: [Option<RunId>; SizeClasses::COUNT],
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
            available: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) fn allocate(
        &mut self,
        spec: LayoutSpec,
        class: SizeClass,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let class_index = class.id().index();

        if let Some(allocation) = self.allocate_from_available(class_index, spec) {
            return Some(allocation);
        }

        let mapping = crate::memory::OsMemory::map(RUN_SIZE)?;
        let reservation = self.runs.reserve()?;
        let id = reservation.id();

        let run = Run::new(id, mapping, class);
        if self.insert_run(reservation, run, pages).is_err() {
            return None;
        }

        let inserted_run = self.runs.get_mut(id)?;
        let ptr = inserted_run.allocate(spec)?;

        if inserted_run.has_available_blocks() {
            self.push_available(class_index, id).ok()?;
        }

        Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing))
    }

    pub(crate) fn free(&mut self, id: RunId, ptr: NonNull<u8>) -> Result<(), RunAllocatorError> {
        let (class_index, was_full) = {
            let Some(run) = self.runs.get_mut(id) else {
                return Err(RunAllocatorError::MissingRun);
            };

            let was_full = run.is_full();
            run.free(ptr).map_err(RunAllocatorError::from)?;

            (run.class().index(), was_full)
        };

        if was_full {
            self.push_available(class_index, id)?;
        }

        Ok(())
    }

    pub(crate) fn resize_in_place(
        &self,
        id: RunId,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, RunAllocatorError> {
        let Some(run) = self.runs.get(id) else {
            return Err(RunAllocatorError::MissingRun);
        };

        run.resize_in_place(ptr, spec)
            .map_err(RunAllocatorError::from)
    }

    fn allocate_from_available(
        &mut self,
        class_index: usize,
        spec: LayoutSpec,
    ) -> Option<Allocation> {
        loop {
            let id = *self.available.get(class_index)?.as_ref()?;
            let (ptr, remains_available, next) = {
                let run = self.runs.get_mut(id)?;
                let ptr = run.allocate(spec);
                let remains_available = ptr.is_some() && run.has_available_blocks();
                let next = if remains_available {
                    run.available_next()
                } else {
                    run.take_available_next()
                };

                (ptr, remains_available, next)
            };

            if !remains_available {
                let available = self.available.get_mut(class_index)?;
                *available = next;
            }

            if let Some(ptr) = ptr {
                return Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing));
            }
        }
    }

    fn push_available(&mut self, class_index: usize, id: RunId) -> Result<(), RunAllocatorError> {
        let Some(available) = self.available.get_mut(class_index) else {
            return Err(RunAllocatorError::InvalidMetadata);
        };

        let Some(run) = self.runs.get_mut(id) else {
            return Err(RunAllocatorError::MissingRun);
        };

        if !run.has_available_blocks() {
            return Err(RunAllocatorError::InvalidMetadata);
        }

        run.set_available_next(*available);
        *available = Some(id);

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
    fn allocator_relinks_previously_full_run_after_free() {
        let mut allocator = RunAllocator::new(2);
        let mut pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::get(spec).unwrap();
        let class_index = class.id().index();
        let capacity = RUN_SIZE / class.block_size();
        let id = RunId::from_index(0).unwrap();
        let first = allocator.allocate(spec, class, &mut pages).unwrap().ptr();

        for _ in 1..capacity {
            assert!(allocator.allocate(spec, class, &mut pages).is_some());
        }

        assert_eq!(allocator.available[class_index], None);
        assert_eq!(allocator.free(id, first), Ok(()));
        assert_eq!(allocator.available[class_index], Some(id));

        let reused = allocator.allocate(spec, class, &mut pages).unwrap().ptr();

        assert_eq!(reused, first);
        assert_eq!(allocator.available[class_index], None);
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
