use core::ptr::NonNull;

use crate::{
    allocation::{Allocation, ZeroStatus},
    config::{RunConfig, RunPolicy},
    layout::LayoutSpec,
    memory::{AddressRange, L2TablePolicy, OsMemory, PageMap, PageOwner, PageRange},
    run::{RUN_SIZE, Run, RunArena, RunBlock, RunError, RunId},
    size_class::{SizeClassId, SizeClasses},
};

use super::{RunReservation, cache::RunCache};

pub(crate) struct RunHeap {
    runs: RunArena,
    available: [Option<NonNull<Run>>; SizeClasses::COUNT],
    cache: RunCache,
    config: RunConfig,
}

#[derive(Clone, Copy)]
struct FreedRun {
    id: RunId,
    class: SizeClassId,
    range: AddressRange,
    owner: NonNull<Run>,
    was_full: bool,
    is_empty: bool,
}

// SAFETY: RunHeap owns run metadata and available-list pointers into its own
// arena. Moving the heap to another thread does not permit concurrent mutation;
// global allocator access remains synchronized by the allocator boundary.
unsafe impl Send for RunHeap {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunHeapError {
    InvalidPointer,
    DoubleFree,
    InvalidMetadata,
}

impl RunHeap {
    pub(crate) const fn new(capacity: u32, config: RunConfig) -> Self {
        Self {
            runs: RunArena::new(capacity),
            available: [None; SizeClasses::COUNT],
            cache: RunCache::new(config),
            config,
        }
    }

    pub(crate) fn allocate(
        &mut self,
        class: SizeClassId,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let class_index = class.index();

        if let Some(allocation) = self.allocate_from_available(class_index) {
            return Some(allocation);
        }

        self.allocate_new_run(class, pages)
    }

    #[cold]
    fn allocate_new_run(&mut self, class: SizeClassId, pages: &mut PageMap) -> Option<Allocation> {
        let class = SizeClasses::class(class)?;
        let class_index = class.id().index();
        let mapping = self
            .cache
            .take(class.id())
            .or_else(|| OsMemory::map(RUN_SIZE))?;
        let reservation = self.runs.reserve()?;
        let id = reservation.id();

        let run = Run::new(id, mapping, class);
        let run_ptr = self.insert_run(reservation, run, pages)?;
        // SAFETY: insert_run returns a pointer to the newly published live RunArena entry.
        let inserted_run = unsafe { &mut *run_ptr.as_ptr() };
        let ptr = inserted_run.allocate()?.ptr();

        if inserted_run.has_available_blocks() {
            self.push_available(class_index, run_ptr).ok()?;
        }

        Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing))
    }

    pub(crate) fn free(
        &mut self,
        owner: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), RunHeapError> {
        let freed = Self::free_block(owner, ptr)?;

        if self.should_release_empty_run(&freed) {
            return self.release_empty_run(freed, pages);
        }

        if freed.was_full {
            self.push_available(freed.class.index(), freed.owner)?;
        }

        Ok(())
    }

    fn free_block(mut owner: NonNull<Run>, ptr: NonNull<u8>) -> Result<FreedRun, RunHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        let run = unsafe { owner.as_mut() };

        let was_full = run.is_full();
        run.free(ptr).map_err(RunHeapError::from)?;

        Ok(FreedRun {
            id: run.id(),
            class: run.class(),
            range: run.range(),
            owner: NonNull::from(&mut *run),
            was_full,
            is_empty: run.is_empty(),
        })
    }

    fn should_release_empty_run(&self, freed: &FreedRun) -> bool {
        freed.is_empty && self.config.policy() != RunPolicy::Keep
    }

    fn release_empty_run(
        &mut self,
        freed: FreedRun,
        pages: &mut PageMap,
    ) -> Result<(), RunHeapError> {
        self.unlink_available(freed.class.index(), freed.owner)?;

        let Some(page_range) = PageRange::new(freed.range.base(), freed.range.len()) else {
            return Err(RunHeapError::InvalidMetadata);
        };
        let empty_l2_tables = if self.cache.will_retain() {
            L2TablePolicy::RetainEmpty
        } else {
            L2TablePolicy::ReleaseEmpty
        };
        pages
            .remove(page_range, PageOwner::Run(freed.owner), empty_l2_tables)
            .map_err(|_| RunHeapError::InvalidMetadata)?;

        let Some(run) = self.runs.remove(freed.id) else {
            return Err(RunHeapError::InvalidMetadata);
        };
        let mapping = run.into_mapping();
        if let Err(mapping) = self.cache.insert(mapping, freed.class) {
            drop(mapping);
        }

        Ok(())
    }

    pub(crate) fn resize_in_place(
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, RunHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        let run = unsafe { run.as_ref() };

        run.resize_in_place(ptr, spec).map_err(RunHeapError::from)
    }

    fn allocate_from_available(&mut self, class_index: usize) -> Option<Allocation> {
        loop {
            let mut run_ptr = *self.available.get(class_index)?.as_ref()?;
            let (ptr, next) = {
                // SAFETY: available-list pointers are created only from live RunArena entries.
                let run = unsafe { run_ptr.as_mut() };
                let ptr = run.allocate().map(RunBlock::ptr);

                if ptr.is_some() && run.has_available_blocks() {
                    return ptr.map(|ptr| Allocation::new(ptr, ZeroStatus::NeedsZeroing));
                }

                (ptr, run.take_available_next())
            };

            let available = self.available.get_mut(class_index)?;
            *available = next;

            if let Some(ptr) = ptr {
                return Some(Allocation::new(ptr, ZeroStatus::NeedsZeroing));
            }
        }
    }

    fn push_available(
        &mut self,
        class_index: usize,
        mut run_ptr: NonNull<Run>,
    ) -> Result<(), RunHeapError> {
        let Some(available) = self.available.get_mut(class_index) else {
            return Err(RunHeapError::InvalidMetadata);
        };

        // SAFETY: caller supplies a pointer derived from this allocator's live RunArena.
        let run = unsafe { run_ptr.as_mut() };

        if !run.has_available_blocks() {
            return Err(RunHeapError::InvalidMetadata);
        }

        run.set_available_next(*available);
        *available = Some(run_ptr);

        Ok(())
    }

    fn unlink_available(
        &mut self,
        class_index: usize,
        target: NonNull<Run>,
    ) -> Result<(), RunHeapError> {
        let Some(head) = self.available.get_mut(class_index) else {
            return Err(RunHeapError::InvalidMetadata);
        };
        let mut current = *head;
        let mut previous: Option<NonNull<Run>> = None;

        while let Some(mut run_ptr) = current {
            // SAFETY: available-list pointers are created only from live RunArena entries.
            let run = unsafe { run_ptr.as_mut() };
            let next = run.available_next();

            if run_ptr == target {
                let _ = run.take_available_next();
                if let Some(mut previous_ptr) = previous {
                    // SAFETY: previous_ptr was read from the same live available list.
                    unsafe { previous_ptr.as_mut() }.set_available_next(next);
                } else {
                    *head = next;
                }
                return Ok(());
            }

            previous = Some(run_ptr);
            current = next;
        }

        Ok(())
    }

    fn insert_run(
        &mut self,
        reservation: RunReservation,
        run: Run,
        pages: &mut PageMap,
    ) -> Option<NonNull<Run>> {
        let id = reservation.id();
        let range = run.range();

        if self.runs.insert(reservation, run).is_err() {
            return None;
        }

        let Some(page_range) = PageRange::new(range.base(), range.len()) else {
            let _removed = self.runs.remove(id);
            return None;
        };

        let Some(inserted_run) = self.runs.get_mut(id) else {
            let _removed = self.runs.remove(id);
            return None;
        };
        let run_ptr = NonNull::from(&mut *inserted_run);

        if pages.insert(page_range, PageOwner::Run(run_ptr)).is_err() {
            let _removed = self.runs.remove(id);
            return None;
        }

        Some(run_ptr)
    }
}

impl From<RunError> for RunHeapError {
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
        config::{Budget, RunPolicy},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner, PageRange},
        run::{RUN_SIZE, Run, RunId},
        size_class::SizeClasses,
    };

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();

        Run::new(id, mapping, class)
    }

    fn available_run_id(allocator: &RunHeap, class_index: usize) -> Option<RunId> {
        allocator.available[class_index].map(|run| {
            // SAFETY: test observes pointers stored by the allocator's live available list.
            unsafe { run.as_ref().id() }
        })
    }

    #[test]
    fn run_heap_relinks_previously_full_run_after_free() {
        let mut allocator = RunHeap::new(2, RunConfig::new());
        let mut pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let class_index = class.id().index();
        let capacity = RUN_SIZE / class.block_size();
        let first = allocator.allocate(class.id(), &mut pages).unwrap().ptr();
        let PageOwner::Run(run_ptr) = pages.get(first).unwrap() else {
            panic!("small allocation should publish a run entry");
        };
        // SAFETY: run_ptr came from the allocator's live page map entry above.
        let id = unsafe { run_ptr.as_ref().id() };

        for _ in 1..capacity {
            assert!(allocator.allocate(class.id(), &mut pages).is_some());
        }

        assert_eq!(available_run_id(&allocator, class_index), None);
        assert_eq!(allocator.free(run_ptr, first, &mut pages), Ok(()));
        assert_eq!(available_run_id(&allocator, class_index), Some(id));

        let reused = allocator.allocate(class.id(), &mut pages).unwrap().ptr();

        assert_eq!(reused, first);
        assert_eq!(available_run_id(&allocator, class_index), None);
    }

    #[test]
    fn failed_run_page_publication_removes_table_entry() {
        let mut allocator = RunHeap::new(4, RunConfig::new());
        let mut pages = PageMap::new();
        let reservation = allocator.runs.reserve().unwrap();
        let id = reservation.id();
        let run = reusable_run(id);
        let range = run.range();
        let page_range = PageRange::new(range.base(), range.len()).unwrap();
        let existing = PageOwner::Run(NonNull::dangling());

        pages.insert(page_range, existing).unwrap();

        assert_eq!(allocator.insert_run(reservation, run, &mut pages), None);
        assert!(allocator.runs.get_mut(id).is_none());
        assert_eq!(pages.get(range.base()), Some(existing));
    }

    #[test]
    fn run_heap_drop_empty_removes_page_map_entry() {
        let mut allocator = RunHeap::new(2, RunConfig::new().with_policy(RunPolicy::DropEmpty));
        let mut pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let ptr = allocator.allocate(class.id(), &mut pages).unwrap().ptr();
        let PageOwner::Run(run_ptr) = pages.get(ptr).unwrap() else {
            panic!("small allocation should publish a run entry");
        };

        assert_eq!(allocator.free(run_ptr, ptr, &mut pages), Ok(()));
        assert_eq!(pages.get(ptr), None);
    }

    #[test]
    fn run_heap_reuses_empty_run_mapping_from_cache() {
        let mut allocator = RunHeap::new(
            2,
            RunConfig::new()
                .with_policy(RunPolicy::RetainFifo)
                .with_budget(Budget::new(1, RUN_SIZE)),
        );
        let mut pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let first = allocator.allocate(class.id(), &mut pages).unwrap().ptr();
        let PageOwner::Run(run_ptr) = pages.get(first).unwrap() else {
            panic!("small allocation should publish a run entry");
        };

        assert_eq!(allocator.free(run_ptr, first, &mut pages), Ok(()));

        let second = allocator.allocate(class.id(), &mut pages).unwrap().ptr();
        assert_eq!(second, first);
    }
}
