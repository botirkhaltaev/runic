use core::ptr::NonNull;

use crate::{
    arena::Arena,
    heap::{HeapError, HeapId, Run, RunId},
    memory::{OsMemory, PageMap},
    size_class::{SizeClass, SizeClasses},
};

pub(crate) struct RunHeap {
    runs: Arena<Run>,
    available: [Option<NonNull<Run>>; SizeClasses::COUNT],
}

// SAFETY: RunHeap owns run metadata and available-list pointers into its own
// arena. Moving the heap to another thread does not permit concurrent mutation;
// global allocator access remains synchronized by the allocator boundary.
unsafe impl Send for RunHeap {}

impl RunHeap {
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            runs: Arena::new(capacity),
            available: [None; SizeClasses::COUNT],
        }
    }

    /// Checkout a run for `class`: available list or cold mmap.
    pub(crate) fn acquire(
        &mut self,
        class: SizeClass,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        self.take_available(class)
            .or_else(|| self.map_run(class, heap_id, pages))
    }

    #[cold]
    fn map_run(
        &mut self,
        class: SizeClass,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        let mapping = OsMemory::map(Run::mapping_len(class)?)?;
        let index = self.runs.claim()?;
        let Some(id) = RunId::from_index(index) else {
            self.runs.release(index);
            return None;
        };

        let Some(run) = Run::new(id, heap_id, mapping, class) else {
            self.runs.release(index);
            return None;
        };
        self.insert_run(index, id, run, pages)
    }

    pub(crate) fn free(&mut self, run: NonNull<Run>, ptr: NonNull<u8>) -> Result<(), HeapError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live arena.
        let run_ref = unsafe { run.as_ref() };
        let was_full = run_ref.is_full();
        run_ref.free(ptr)?;
        if was_full {
            self.push_available(run)?;
        }
        Ok(())
    }

    /// Owner: drain every claimed bit on `run` and publish the freed blocks.
    ///
    /// Returns whether the caller must `Inbox::push` `run` again because a straggling claim
    /// raced the scan (see `Run::accept`).
    pub(crate) fn accept(&mut self, run: NonNull<Run>) -> Result<bool, HeapError> {
        // SAFETY: the run inbox only ever carries pointers published from this allocator's
        // live arena.
        let run_ref = unsafe { run.as_ref() };
        let was_full = run_ref.is_full();
        let needs_push = run_ref.accept();
        if was_full && !run_ref.is_full() {
            self.push_available(run)?;
        }
        Ok(needs_push)
    }

    pub(crate) fn rebind(&mut self, heap_id: HeapId) {
        debug_assert!(self.runs.len() <= self.runs.capacity());
        let len = self.runs.len();
        for index in 0..len {
            let Some(run) = self.runs.get_mut(index) else {
                continue;
            };
            run.set_heap_id(heap_id);
        }
    }

    /// Any occupied run with outstanding allocated or claimed blocks.
    pub(crate) fn has_live(&self) -> bool {
        let len = self.runs.len();
        for index in 0..len {
            if self.runs.get(index).is_some_and(Run::is_live) {
                return true;
            }
        }
        false
    }

    pub(crate) fn push_available(&mut self, mut run_ptr: NonNull<Run>) -> Result<(), HeapError> {
        // SAFETY: caller supplies a pointer derived from this allocator's live arena.
        let run = unsafe { run_ptr.as_mut() };
        if run.is_full() {
            return Err(HeapError::InvalidMetadata);
        }
        let Some(available) = self.available.get_mut(run.class().index()) else {
            return Err(HeapError::InvalidMetadata);
        };
        run.set_available_next(*available);
        *available = Some(run_ptr);
        Ok(())
    }

    fn take_available(&mut self, class: SizeClass) -> Option<NonNull<Run>> {
        let class_index = class.index();
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
            if !unsafe { run_ptr.as_ref() }.is_full() {
                return Some(run_ptr);
            }
        }
    }

    fn insert_run(
        &mut self,
        index: u32,
        id: RunId,
        run: Run,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        if self.runs.insert(index, run).is_none() {
            self.runs.release(index);
            return None;
        }

        let Some(inserted_run) = self.runs.get_mut(index) else {
            let _removed = self.runs.remove(id.index());
            return None;
        };
        debug_assert_eq!(inserted_run.id(), id);
        let run_ptr = NonNull::from(&mut *inserted_run);

        debug_assert_eq!(inserted_run.range().base(), inserted_run.mapping().base());
        if pages.publish_run(inserted_run.mapping(), run_ptr).is_err() {
            let _removed = self.runs.remove(id.index());
            return None;
        }

        Some(run_ptr)
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use crate::{
        heap::{HeapId, Run, RunId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
        size_class::SizeClasses,
    };

    use super::super::RUN_SIZE;
    use super::*;

    fn class_id(size: usize, align: usize) -> SizeClass {
        SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(size, align).unwrap(),
        ))
        .unwrap()
    }

    fn reusable_run(id: RunId) -> Run {
        let class = class_id(64, 8);
        let mapping = OsMemory::map(Run::mapping_len(class).unwrap()).unwrap();
        let heap = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();

        Run::new(id, heap, mapping, class).expect("reusable test run")
    }

    fn available_run_id(heap: &RunHeap, class_index: usize) -> Option<RunId> {
        heap.available[class_index].map(|run| {
            // SAFETY: test observes pointers stored by the heap's live available list.
            unsafe { run.as_ref().id() }
        })
    }

    fn alloc_block(
        heap: &mut RunHeap,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<(NonNull<Run>, NonNull<u8>)> {
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let mut run = heap.acquire(class, heap_id, pages)?;
        // SAFETY: RunHeap returns pointers to live runs from its arena.
        let ptr = unsafe { run.as_mut() }.allocate()?;
        // SAFETY: RunHeap returns pointers to live runs from its arena.
        if !unsafe { run.as_ref() }.is_full() {
            heap.push_available(run).ok()?;
        }
        Some((run, ptr))
    }

    #[test]
    fn run_heap_relinks_previously_full_run_after_free() {
        let mut heap = RunHeap::new(2);
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();
        let capacity = RUN_SIZE / class.size();
        let (_run, first) = alloc_block(&mut heap, class, &pages).unwrap();
        let PageOwner::Run(run_ptr) = pages.get(first).unwrap() else {
            panic!("small allocation should publish a run entry");
        };
        // SAFETY: run_ptr came from the allocator's live page map entry above.
        let id = unsafe { run_ptr.as_ref().id() };

        for _ in 1..capacity {
            assert!(alloc_block(&mut heap, class, &pages).is_some());
        }

        assert_eq!(available_run_id(&heap, class_index), None);
        assert_eq!(heap.free(run_ptr, first), Ok(()));
        assert_eq!(available_run_id(&heap, class_index), Some(id));

        let (_run, reused) = alloc_block(&mut heap, class, &pages).unwrap();

        assert_eq!(reused, first);
        assert_eq!(available_run_id(&heap, class_index), None);
    }

    #[test]
    fn failed_run_page_publication_removes_map_entry() {
        let mut heap = RunHeap::new(4);
        let pages = PageMap::new();
        let index = heap.runs.claim().unwrap();
        let id = RunId::from_index(index).unwrap();
        assert_eq!(id.index(), index);
        let run = reusable_run(id);
        let existing = NonNull::dangling();
        let base = run.range().base();

        pages.publish_run(run.mapping(), existing).unwrap();

        assert_eq!(heap.insert_run(index, id, run, &pages), None);
        assert!(heap.runs.get_mut(index).is_none());
        assert_eq!(pages.get(base), Some(PageOwner::Run(existing)));
    }

    #[test]
    fn rebind_rebinds_runs_off_the_available_list() {
        let mut heap = RunHeap::new(2);
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let old = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let new = HeapId::new(0, core::num::NonZeroU32::new(2).unwrap()).unwrap();

        let run = heap.acquire(class, old, &pages).unwrap();
        // Leave the run checked out (sticky-style): never push_available.
        // SAFETY: run came from this heap's live arena.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), old);

        heap.rebind(new);

        // SAFETY: run remains a live arena entry after rebind.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), new);
    }
}
