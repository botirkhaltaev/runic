use core::ptr::NonNull;

use crate::{
    arena::Arena,
    heap::{Heap, HeapError, Run, RunId},
    memory::{Mapping, Memory, Os, PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

use super::{
    Accept, MAP_RUNS, MAP_SIZE, RUN_SIZE, RUN_SPACE, RunFree,
    config::{RunConfig, RunPolicy},
};
use crate::config::Hints;

/// Owner-local run directory and available lists.
///
/// Run mappings stay live for the process. Headers live at `base + RUN_SIZE`.
/// Aggregate live counts live on [`Heap`].
pub(crate) struct RunHeap {
    /// In-space run headers. The `Run` itself lives at `base + RUN_SIZE`.
    runs: Arena<&'static Run>,
    maps: Arena<Mapping>,
    map_index: Option<u32>,
    used: usize,
    available: [Option<&'static Run>; SizeClasses::COUNT],
    policy: RunPolicy,
    hints: Hints,
}

impl RunHeap {
    pub(crate) const fn new(config: RunConfig, hints: Hints) -> Self {
        Self {
            runs: Arena::new(),
            maps: Arena::new(),
            map_index: None,
            used: 0,
            available: [None; SizeClasses::COUNT],
            policy: config.policy(),
            hints,
        }
    }

    /// Checkout a run for `class`: available list or a new range in a heap map.
    pub(crate) fn acquire(
        &mut self,
        class: SizeClass,
        heap: &'static Heap,
        pages: &PageMap,
    ) -> Option<&'static Run> {
        self.take_available(class)
            .or_else(|| self.new_run(class, heap, pages))
    }

    #[cold]
    fn new_run(
        &mut self,
        class: SizeClass,
        heap: &'static Heap,
        pages: &PageMap,
    ) -> Option<&'static Run> {
        let base = self.take()?;
        let index = self.runs.vacant()?;
        let id = RunId::from_index(index)?;
        let run = Run::new(id, heap, base, class, self.policy)?;
        let header = self.insert_run(run, pages)?;
        self.used += 1;
        Some(header)
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
        let mapping = Os::map_aligned_payload(MAP_SIZE, RUN_SIZE, self.hints)?;
        let inserted = self.maps.insert(index, mapping)?;
        self.map_index = Some(index);
        self.used = 0;
        Some(inserted.base())
    }

    /// Owner: drain every claimed bit on `run` and publish the freed blocks.
    ///
    /// Returns `Requeue` when the caller must queue `run` again because a straggling claim
    /// raced the scan (see `Run::accept`).
    pub(crate) fn accept(&mut self, run: &'static Run) -> Result<Accept, HeapError> {
        let was_full = run.is_full();
        let accept = run.accept();
        if was_full && !run.is_full() {
            self.push_available(run)?;
        }
        Ok(accept)
    }

    /// Any occupied run with outstanding allocated or claimed blocks.
    ///
    /// Production reclaim uses [`Heap::occupied`] then this scan.
    pub(crate) fn has_live(&self) -> bool {
        self.runs.iter().any(|run| run.is_live())
    }

    #[inline(never)]
    pub(crate) fn push_available(&mut self, run: &'static Run) -> Result<(), HeapError> {
        if run.listed() {
            return Ok(());
        }
        if run.is_full() {
            return Err(HeapError::InvalidMetadata);
        }
        let Some(available) = self.available.get_mut(run.class().index()) else {
            return Err(HeapError::InvalidMetadata);
        };
        run.list_available(*available);
        *available = Some(run);
        Ok(())
    }

    /// Slow-path owner free: list a run that left full, then Discard an empty payload.
    pub(crate) fn release(&mut self, run: &'static Run, outcome: RunFree) -> Result<(), HeapError> {
        if outcome == RunFree::Available {
            self.push_available(run)?;
        }
        if run.is_discardable() {
            run.discard();
        }
        Ok(())
    }

    fn take_available(&mut self, class: SizeClass) -> Option<&'static Run> {
        let class_index = class.index();
        loop {
            let run = (*self.available.get(class_index)?)?;
            let next = run.unlist_available();
            let full = run.is_full();

            let available = self.available.get_mut(class_index)?;
            *available = next;

            if !full {
                return Some(run);
            }
        }
    }

    fn insert_run(&mut self, run: Run, pages: &PageMap) -> Option<&'static Run> {
        let index = run.id().index();
        let header: NonNull<Run> = NonNull::new(
            run.range()
                .base()
                .as_ptr()
                .wrapping_byte_add(RUN_SIZE)
                .cast(),
        )?;
        // SAFETY: `header` is in this run's mapped tail; first write to this space.
        unsafe { header.as_ptr().write(run) };
        // SAFETY: header was just written. Run mappings stay live for the process.
        let written = unsafe { &*header.as_ptr() };
        self.runs.insert(index, written)?;

        if pages.publish(PageOwner::Run(written)).is_err() {
            let _removed = self.runs.remove(index);
            written.poison();
            return None;
        }

        Some(written)
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use crate::{
        config::{AllocatorConfig, Hints},
        heap::{Heap, HeapId, Run, RunId},
        layout::LayoutSpec,
        memory::{PageMap, PageOwner},
        size_class::SizeClasses,
    };

    use super::super::{MAP_RUNS, RUN_SIZE, RunFree, config::RunConfig};
    use super::*;

    static OWNER: Heap = Heap::new(
        HeapId::new(0, core::num::NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );

    fn class_id(size: usize, align: usize) -> SizeClass {
        SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(size, align).unwrap(),
        ))
        .unwrap()
    }

    fn available_run_id(heap: &RunHeap, class_index: usize) -> Option<RunId> {
        heap.available[class_index].map(Run::id)
    }

    fn alloc_block(
        heap: &mut RunHeap,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<(&'static Run, NonNull<u8>)> {
        let run = heap.acquire(class, &OWNER, pages)?;
        let ptr = run.allocate().or_else(|| {
            run.extend();
            run.allocate()
        })?;
        if !run.is_full() {
            heap.push_available(run).ok()?;
        }
        Some((run, ptr))
    }

    #[test]
    fn run_heap_relinks_previously_full_run_after_free() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();
        let (_run, first) = alloc_block(&mut heap, class, &pages).unwrap();
        let run = pages
            .get(first)
            .and_then(|owner| match owner {
                PageOwner::Run(run) => Some(run),
                PageOwner::Extent(_) => None,
            })
            .expect("small allocation should publish a run entry");
        let id = run.id();

        for _ in 1..RUN_SIZE / class.size() {
            assert!(alloc_block(&mut heap, class, &pages).is_some());
        }

        assert_eq!(available_run_id(&heap, class_index), None);
        assert_eq!(run.free(first), Ok(RunFree::Available));
        assert_eq!(heap.push_available(run), Ok(()));
        assert_eq!(available_run_id(&heap, class_index), Some(id));

        let (_run, reused) = alloc_block(&mut heap, class, &pages).unwrap();

        assert_eq!(reused, first);
        assert_eq!(available_run_id(&heap, class_index), None);
    }

    #[test]
    fn failed_run_page_publication_leaves_range_reusable() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        // Occupy the range first; `insert_run` must fail closed rather than steal it.
        let mut occupant = RunHeap::new(RunConfig::new(), Hints::new());
        let taken = occupant.acquire(class, &OWNER, &pages).unwrap();
        let base = taken.range().base();

        let index = heap.runs.vacant().unwrap();
        let id = RunId::from_index(index).unwrap();
        let run = Run::new(id, &OWNER, base, class, RunPolicy::Keep).expect("conflict run");
        assert!(heap.insert_run(run, &pages).is_none());

        assert!(heap.runs.get(index).is_none());
        assert_eq!(pages.get(base), Some(PageOwner::Run(taken)));
        // The rejected header is poisoned, so the range no longer resolves an owner.
        assert!(Run::header_of(base).is_none());
    }

    #[test]
    fn push_available_is_idempotent_and_keeps_tail() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();

        let run_a = heap.acquire(class, &OWNER, &pages).unwrap();
        let run_b = heap.acquire(class, &OWNER, &pages).unwrap();
        let id_a = run_a.id();
        let id_b = run_b.id();
        assert_ne!(id_a, id_b);

        assert_eq!(heap.push_available(run_a), Ok(()));
        assert_eq!(heap.push_available(run_b), Ok(()));
        assert_eq!(heap.push_available(run_a), Ok(()));

        let first = heap.acquire(class, &OWNER, &pages).unwrap();
        assert_eq!(first.id(), id_b);
        let second = heap.acquire(class, &OWNER, &pages).unwrap();
        assert_eq!(second.id(), id_a);
        assert_eq!(available_run_id(&heap, class_index), None);
    }

    #[test]
    fn push_available_returns_stranded_current() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let class_index = class.index();

        let run = heap.acquire(class, &OWNER, &pages).unwrap();
        let ptr = run
            .allocate()
            .or_else(|| {
                run.extend();
                run.allocate()
            })
            .unwrap();
        let id = run.id();
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        assert_eq!(available_run_id(&heap, class_index), None);

        assert_eq!(heap.push_available(run), Ok(()));
        assert_eq!(available_run_id(&heap, class_index), Some(id));

        let (_run, reused) = alloc_block(&mut heap, class, &pages).unwrap();
        assert_eq!(reused, ptr);
    }

    #[test]
    fn publish_run_covers_payload_not_claim_tail() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = heap.acquire(class, &OWNER, &pages).unwrap();
        let base = run.range().base();
        assert!(pages.get(base).is_some());
        let tail = NonNull::new(base.as_ptr().wrapping_byte_add(RUN_SIZE)).unwrap();
        assert!(pages.get(tail).is_none());
    }

    #[test]
    fn sixteen_runs_share_one_map() {
        let mut heap = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        for _ in 0..MAP_RUNS {
            assert!(heap.acquire(class, &OWNER, &pages).is_some());
        }
        assert_eq!(heap.maps.iter().count(), 1);
        assert!(heap.acquire(class, &OWNER, &pages).is_some());
        assert_eq!(heap.maps.iter().count(), 2);
    }
}
