use core::ptr::NonNull;

use crate::{
    arena::Arena,
    heap::{HeapError, HeapId, Run, RunId},
    memory::{Mapping, OsMemory, PageMap},
    size_class::{SizeClass, SizeClasses},
};

use super::{
    MAP_RUNS, MAP_SIZE, RUN_SIZE, RUN_SPACE,
    config::{RunConfig, RunPolicy},
};

pub(crate) struct RunHeap {
    runs: Arena<Run>,
    maps: Arena<Mapping>,
    map_index: Option<u32>,
    used: usize,
    available: [Option<NonNull<Run>>; SizeClasses::COUNT],
    policy: RunPolicy,
}

// SAFETY: RunHeap owns run metadata and available-list pointers into its own
// arena. Moving the heap to another thread does not permit concurrent mutation;
// global allocator access remains synchronized by the allocator boundary.
unsafe impl Send for RunHeap {}

impl RunHeap {
    pub(crate) fn new(config: RunConfig) -> Self {
        Self {
            runs: Arena::new(),
            maps: Arena::new(),
            map_index: None,
            used: 0,
            available: [None; SizeClasses::COUNT],
            policy: config.policy(),
        }
    }

    /// Checkout a run for `class`: available list or a new range in a heap map.
    pub(crate) fn acquire(
        &mut self,
        class: SizeClass,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        self.take_available(class)
            .or_else(|| self.new_run(class, heap_id, pages))
    }

    #[cold]
    fn new_run(
        &mut self,
        class: SizeClass,
        heap_id: HeapId,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        let base = self.take()?;
        let index = self.runs.vacant()?;
        let id = RunId::from_index(index)?;
        let run = Run::new(id, heap_id, base, class, self.policy)?;
        let run = self.insert_run(index, id, run, pages)?;
        self.used += 1;
        Some(run)
    }

    fn take(&mut self) -> Option<NonNull<u8>> {
        if let Some(index) = self.map_index
            && self.used < MAP_RUNS
            && let Some(mapping) = self.maps.get(index)
        {
            let base = mapping
                .base()
                .as_ptr()
                .wrapping_byte_add(self.used * RUN_SPACE);
            return NonNull::new(base);
        }
        self.map()
    }

    fn map(&mut self) -> Option<NonNull<u8>> {
        let index = self.maps.vacant()?;
        let mapping = OsMemory::map_aligned(MAP_SIZE, RUN_SIZE)?;
        let inserted = self.maps.insert(index, mapping)?;
        self.map_index = Some(index);
        self.used = 0;
        Some(inserted.base())
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
        for run in self.runs.iter_mut() {
            run.set_heap_id(heap_id);
        }
    }

    /// Any occupied run with outstanding allocated or claimed blocks.
    pub(crate) fn has_live(&self) -> bool {
        self.runs.iter().any(Run::is_live)
    }

    #[inline(never)]
    pub(crate) fn push_available(&mut self, mut run_ptr: NonNull<Run>) -> Result<(), HeapError> {
        // SAFETY: caller supplies a pointer derived from this allocator's live arena.
        let run = unsafe { run_ptr.as_mut() };
        if run.is_available() {
            return Ok(());
        }
        if run.is_full() {
            return Err(HeapError::InvalidMetadata);
        }
        let Some(available) = self.available.get_mut(run.class().index()) else {
            return Err(HeapError::InvalidMetadata);
        };
        run.link_available(*available);
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
                run.unlink_available()
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
        let inserted_run = self.runs.insert(index, run)?;
        debug_assert_eq!(inserted_run.id(), id);
        let run_ptr = NonNull::from(&mut *inserted_run);

        if pages.publish_run(inserted_run.range(), run_ptr).is_err() {
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

    use super::super::{MAP_RUNS, RUN_SIZE, RUN_SPACE, config::RunConfig};
    use super::*;

    fn class_id(size: usize, align: usize) -> SizeClass {
        SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(size, align).unwrap(),
        ))
        .unwrap()
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
        let run_ref = unsafe { run.as_mut() };
        let ptr = run_ref.allocate().or_else(|| {
            run_ref.extend();
            run_ref.allocate()
        })?;
        // SAFETY: RunHeap returns pointers to live runs from its arena.
        if !unsafe { run.as_ref() }.is_full() {
            heap.push_available(run).ok()?;
        }
        Some((run, ptr))
    }

    #[test]
    fn run_heap_relinks_previously_full_run_after_free() {
        let mut heap = RunHeap::new(RunConfig::new());
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
        // SAFETY: run_ptr is the live page-map run we just filled.
        assert_eq!(unsafe { run_ptr.as_ref() }.free(first), Ok(true));
        assert_eq!(heap.push_available(run_ptr), Ok(()));
        assert_eq!(available_run_id(&heap, class_index), Some(id));

        let (_run, reused) = alloc_block(&mut heap, class, &pages).unwrap();

        assert_eq!(reused, first);
        assert_eq!(available_run_id(&heap, class_index), None);
    }

    #[test]
    fn failed_run_page_publication_leaves_range_reusable() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let mapping = OsMemory::map_aligned(RUN_SPACE, RUN_SIZE).unwrap();
        let existing = NonNull::dangling();
        let range = crate::memory::AddressRange::new(mapping.base(), RUN_SIZE);
        pages.publish_run(range, existing).unwrap();

        let index = heap.runs.vacant().unwrap();
        let id = RunId::from_index(index).unwrap();
        let run =
            Run::new(id, heap_id, mapping.base(), class, RunPolicy::Keep).expect("conflict run");
        assert_eq!(heap.insert_run(index, id, run, &pages), None);
        assert!(heap.runs.get_mut(index).is_none());
        assert_eq!(pages.get(mapping.base()), Some(PageOwner::Run(existing)));
    }

    #[test]
    fn rebind_rebinds_runs_off_the_available_list() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let old = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let new = HeapId::new(0, core::num::NonZeroU32::new(2).unwrap()).unwrap();

        let run = heap.acquire(class, old, &pages).unwrap();
        // Leave the run checked out (not on available): reincarnation still rebinds it.
        // SAFETY: run came from this heap's live arena.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), old);

        heap.rebind(new);

        // SAFETY: run remains a live arena entry after rebind.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), new);
    }

    #[test]
    fn push_available_is_idempotent_and_keeps_tail() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();

        let run_a = heap.acquire(class, heap_id, &pages).unwrap();
        let run_b = heap.acquire(class, heap_id, &pages).unwrap();
        // SAFETY: both from this heap's live arena.
        let id_a = unsafe { run_a.as_ref().id() };
        let id_b = unsafe { run_b.as_ref().id() };
        assert_ne!(id_a, id_b);

        assert_eq!(heap.push_available(run_a), Ok(()));
        assert_eq!(heap.push_available(run_b), Ok(()));
        assert_eq!(heap.push_available(run_a), Ok(()));

        let first = heap.acquire(class, heap_id, &pages).unwrap();
        let second = heap.acquire(class, heap_id, &pages).unwrap();
        // SAFETY: just acquired from this heap.
        assert_eq!(unsafe { first.as_ref().id() }, id_b);
        assert_eq!(unsafe { second.as_ref().id() }, id_a);
        assert_eq!(available_run_id(&heap, class_index), None);
    }

    #[test]
    fn push_available_returns_stranded_current() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();

        let mut run = heap.acquire(class, heap_id, &pages).unwrap();
        // SAFETY: live arena run, exclusive to this test.
        let run_ref = unsafe { run.as_mut() };
        let ptr = run_ref
            .allocate()
            .or_else(|| {
                run_ref.extend();
                run_ref.allocate()
            })
            .unwrap();
        let id = run_ref.id();
        assert_eq!(run_ref.free(ptr), Ok(false));
        assert_eq!(available_run_id(&heap, class_index), None);

        assert_eq!(heap.push_available(run), Ok(()));
        assert_eq!(available_run_id(&heap, class_index), Some(id));

        let (_run, reused) = alloc_block(&mut heap, class, &pages).unwrap();
        assert_eq!(reused, ptr);
    }

    #[test]
    fn publish_run_covers_payload_not_claim_tail() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        let run = heap.acquire(class, heap_id, &pages).unwrap();
        // SAFETY: live arena run.
        let base = unsafe { run.as_ref() }.range().base();
        assert!(pages.get(base).is_some());
        let tail = NonNull::new(base.as_ptr().wrapping_byte_add(RUN_SIZE)).unwrap();
        assert!(pages.get(tail).is_none());
    }

    #[test]
    fn sixteen_runs_share_one_map() {
        let mut heap = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let heap_id = HeapId::new(0, core::num::NonZeroU32::MIN).unwrap();
        for _ in 0..MAP_RUNS {
            assert!(heap.acquire(class, heap_id, &pages).is_some());
        }
        assert_eq!(heap.maps.iter().count(), 1);
        assert!(heap.acquire(class, heap_id, &pages).is_some());
        assert_eq!(heap.maps.iter().count(), 2);
    }
}
