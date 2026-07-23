use core::ptr::NonNull;

use crate::{
    arena::Arena,
    heap::{HeapId, RUN_SIZE, Run, RunError, RunId},
    layout::LayoutSpec,
    memory::{OsMemory, PageMap},
    size_class::{SizeClassId, SizeClasses},
};

pub(crate) struct RunHeap {
    runs: Arena<Run>,
    available: [Option<NonNull<Run>>; SizeClasses::COUNT],
}

#[derive(Clone, Copy)]
struct FreedRun {
    class: SizeClassId,
    run: NonNull<Run>,
    was_full: bool,
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
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            runs: Arena::new(capacity),
            available: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) fn allocate(
        &mut self,
        class: SizeClassId,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        self.take_available(class)
            .or_else(|| self.allocate_run(class, heap_id, pages))
    }

    pub(crate) fn take_available(&mut self, class: SizeClassId) -> Option<NonNull<Run>> {
        self.take_available_from(class.index())
    }

    #[cold]
    pub(crate) fn allocate_run(
        &mut self,
        class: SizeClassId,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        let class = SizeClasses::class(class)?;
        let mapping = OsMemory::map(RUN_SIZE)?;
        let index = self.runs.claim()?;
        let Some(id) = RunId::from_index(u32::try_from(index).ok()?) else {
            self.runs.release(index);
            return None;
        };

        let run = Run::new(id, heap_id, mapping, class);
        self.insert_run(index, id, run, pages)
    }

    pub(crate) fn free(&mut self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), RunHeapError> {
        let freed = Self::free_block(run, ptr)?;
        self.finish_free(freed)
    }

    pub(crate) fn rebind_heap_id(&mut self, heap_id: HeapId) {
        let len = self.runs.capacity();
        for index in 0..len {
            let Some(run) = self.runs.get_mut(index) else {
                continue;
            };
            run.set_heap_id(heap_id);
        }
    }

    pub(crate) fn complete_remote_free(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<(), RunHeapError> {
        let freed = Self::complete_remote_block(run, ptr)?;
        self.finish_free(freed)
    }

    fn free_block(mut run: NonNull<Run>, ptr: NonNull<u8>) -> Result<FreedRun, RunHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_mut() };
        let status = run_ref.free_local(ptr).map_err(RunHeapError::from)?;

        Ok(FreedRun {
            class: run_ref.class(),
            run: NonNull::from(&mut *run_ref),
            was_full: status.was_full(),
        })
    }

    fn complete_remote_block(
        mut run: NonNull<Run>,
        ptr: NonNull<u8>,
    ) -> Result<FreedRun, RunHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_mut() };
        let status = run_ref
            .complete_remote_free(ptr)
            .map_err(RunHeapError::from)?;

        Ok(FreedRun {
            class: run_ref.class(),
            run: NonNull::from(&mut *run_ref),
            was_full: status.was_full(),
        })
    }

    fn finish_free(&mut self, freed: FreedRun) -> Result<(), RunHeapError> {
        if freed.was_full {
            self.push_available(freed.class.index(), freed.run)?;
        }

        Ok(())
    }

    pub(crate) fn resize_in_place(
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, RunHeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run = unsafe { run.as_ref() };

        run.resize_in_place(ptr, spec).map_err(RunHeapError::from)
    }

    pub(crate) fn return_available(
        &mut self,
        mut run_ptr: NonNull<Run>,
    ) -> Result<(), RunHeapError> {
        // SAFETY: caller supplies a pointer derived from this allocator's live arena.
        let run = unsafe { run_ptr.as_mut() };
        self.push_available(run.class().index(), run_ptr)
    }

    fn take_available_from(&mut self, class_index: usize) -> Option<NonNull<Run>> {
        loop {
            let mut run_ptr = *self.available.get(class_index)?.as_ref()?;
            let next = {
                // SAFETY: available-list pointers are created only from live arena entries.
                let run = unsafe { run_ptr.as_mut() };
                run.take_available_next()
            };

            let available = self.available.get_mut(class_index)?;
            *available = next;

            // SAFETY: available-list pointers are created only from live arena entries.
            if unsafe { run_ptr.as_ref() }.has_available_blocks() {
                return Some(run_ptr);
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

        // SAFETY: caller supplies a pointer derived from this allocator's live arena.
        let run = unsafe { run_ptr.as_mut() };

        if !run.has_available_blocks() {
            return Err(RunHeapError::InvalidMetadata);
        }

        run.set_available_next(*available);
        *available = Some(run_ptr);

        Ok(())
    }

    fn insert_run(
        &mut self,
        index: usize,
        id: RunId,
        run: Run,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        let range = run.range();

        if self.runs.insert(index, run).is_none() {
            self.runs.release(index);
            return None;
        }

        let Some(inserted_run) = self.runs.get_mut(index) else {
            let _removed = self.runs.remove(usize::try_from(id.index()).ok()?);
            return None;
        };
        debug_assert_eq!(inserted_run.id, id);
        let run_ptr = NonNull::from(&mut *inserted_run);

        if pages.publish_run(range, run_ptr).is_err() {
            let _removed = self.runs.remove(usize::try_from(id.index()).ok()?);
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
        heap::{HeapId, RUN_SIZE, Run, RunId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
        size_class::SizeClasses,
    };

    use super::*;

    fn reusable_run(id: RunId) -> Run {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let heap = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();

        Run::new(id, heap, mapping, class)
    }

    fn available_run_id(allocator: &RunHeap, class_index: usize) -> Option<RunId> {
        allocator.available[class_index].map(|run| {
            // SAFETY: test observes pointers stored by the allocator's live available list.
            unsafe { run.as_ref().id() }
        })
    }

    fn allocate_block(
        allocator: &mut RunHeap,
        class: SizeClassId,
        pages: &PageMap,
    ) -> Option<(NonNull<Run>, NonNull<u8>)> {
        let heap = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let mut run = allocator.allocate(class, heap, pages)?;
        // SAFETY: RunHeap returns pointers to live runs from its arena.
        let ptr = unsafe { run.as_mut() }.allocate()?;
        // SAFETY: RunHeap returns pointers to live runs from its arena.
        if unsafe { run.as_ref() }.has_available_blocks() {
            allocator.return_available(run).ok()?;
        }
        Some((run, ptr))
    }

    #[test]
    fn run_heap_relinks_previously_full_run_after_free() {
        let mut allocator = RunHeap::new(2);
        let pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let class_index = class.id().index();
        let capacity = RUN_SIZE / class.block_size();
        let (_run, first) = allocate_block(&mut allocator, class.id(), &pages).unwrap();
        let PageOwner::Run(run_ptr) = pages.get(first).unwrap() else {
            panic!("small allocation should publish a run entry");
        };
        // SAFETY: run_ptr came from the allocator's live page map entry above.
        let id = unsafe { run_ptr.as_ref().id() };

        for _ in 1..capacity {
            assert!(allocate_block(&mut allocator, class.id(), &pages).is_some());
        }

        assert_eq!(available_run_id(&allocator, class_index), None);
        assert_eq!(allocator.free(run_ptr, first), Ok(()));
        assert_eq!(available_run_id(&allocator, class_index), Some(id));

        let (_run, reused) = allocate_block(&mut allocator, class.id(), &pages).unwrap();

        assert_eq!(reused, first);
        assert_eq!(available_run_id(&allocator, class_index), None);
    }

    #[test]
    fn failed_run_page_publication_removes_table_entry() {
        let mut allocator = RunHeap::new(4);
        let pages = PageMap::new();
        let index = allocator.runs.claim().unwrap();
        let id = RunId::from_index(u32::try_from(index).unwrap()).unwrap();
        assert_eq!(usize::try_from(id.index()).unwrap(), index);
        let run = reusable_run(id);
        let range = run.range();
        let existing = NonNull::dangling();

        pages.publish_run(range, existing).unwrap();

        assert_eq!(allocator.insert_run(index, id, run, &pages), None);
        assert!(allocator.runs.get_mut(index).is_none());
        assert_eq!(pages.get(range.base()), Some(PageOwner::Run(existing)));
    }

    #[test]
    fn rebind_heap_id_rebinds_runs_off_the_available_list() {
        let mut allocator = RunHeap::new(2);
        let pages = PageMap::new();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let old = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let new = HeapId::new(0, core::num::NonZeroU32::new(2).unwrap()).unwrap();

        let run = allocator.allocate(class.id(), old, &pages).unwrap();
        // Leave the run checked out (sticky-style): never return_available.
        // SAFETY: run came from this heap's live arena.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), old);

        allocator.rebind_heap_id(new);

        // SAFETY: run remains a live arena entry after rebind.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), new);
    }
}
