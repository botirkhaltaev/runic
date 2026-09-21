//! Heap lifecycle / lease tests. Active body + inbox reclaim live in `allocator` tests
//! (require `ThreadHeaps::bind` / `Heaps::{unbind,enqueue,free,flush}`).

use super::*;
use crate::{
    config::AllocatorConfig,
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
};
use core::alloc::Layout;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use state::MAX_LEASES;

static PAGES: OnceLock<PageMap> = OnceLock::new();

fn unbind(heaps: &Heaps, id: HeapId) {
    let pages = PAGES.get_or_init(PageMap::new);
    assert_eq!(heaps.unbind(id, &AllocatorCtx { pages, heaps }), Ok(()));
}

#[test]
fn heap_equality_is_identity() {
    let first = Heap::new(
        HeapId::new(0, core::num::NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );
    let second = Heap::new(
        HeapId::new(1, core::num::NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );

    assert!(first == first);
    assert!(first != second);
}

#[test]
fn lease_rejected_after_close() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    assert!(heap.state.acquire_lease(id).is_err());
    unbind(&heaps, id);
}

#[test]
fn lease_count_overflow_fails_closed() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    // Forge the packed lease ceiling — acquiring `(1<<29)-1` real leases is not practical.
    heap.state
        .store(id.generation(), HeapMode::Active, MAX_LEASES);
    assert!(matches!(
        heap.state.acquire_lease(id),
        Err(HeapError::InvalidMetadata)
    ));
    heap.state.store(id.generation(), HeapMode::Active, 0);
    unbind(&heaps, id);
}

#[test]
fn adopt_promotes_draining_to_active() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.close(id), Ok(()));
    assert_eq!(heap.mode(), HeapMode::Draining);
    let inner = heap.adopt(id).unwrap();
    drop(inner);
    assert_eq!(heap.mode(), HeapMode::Active);
    assert!(matches!(heap.adopt(id), Err(HeapError::InvalidHeap)));
    unbind(&heaps, id);
}

#[test]
fn adopt_race_has_exactly_one_winner() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.close(id), Ok(()));
    let winners = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                if let Ok(inner) = heap.adopt(id) {
                    winners.fetch_add(1, Ordering::Relaxed);
                    drop(inner);
                }
            });
        }
    });

    assert_eq!(winners.load(Ordering::Relaxed), 1);
    unbind(&heaps, id);
}

#[test]
fn lifecycle_rejects_id_from_another_slot() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let first_id = heaps.acquire().unwrap().id();
    let second_id = heaps.acquire().unwrap().id();
    let first = heaps.get(first_id).unwrap();

    assert_eq!(first.close(second_id), Err(HeapError::InvalidHeap));
    assert!(matches!(
        first.adopt(second_id),
        Err(HeapError::InvalidHeap)
    ));
    assert_eq!(first.mode(), HeapMode::Active);

    let pages = PAGES.get_or_init(PageMap::new);
    let ctx = AllocatorCtx {
        pages,
        heaps: &heaps,
    };
    assert_eq!(heaps.unbind(first_id, &ctx), Ok(()));
    assert_eq!(heaps.unbind(second_id, &ctx), Ok(()));
}

#[test]
fn extent_alloc_preserves_flush_error() {
    static HEAPS: OnceLock<Heaps> = OnceLock::new();

    let heaps = HEAPS.get_or_init(|| Heaps::new(AllocatorConfig::new()));
    let pages = PAGES.get_or_init(PageMap::new);
    let ctx = AllocatorCtx { pages, heaps };
    let heap = heaps.acquire().unwrap();
    let spec = LayoutSpec::from_layout(Layout::from_size_align(128 * 1024, 4096).unwrap());
    let mut inner = heap.require_inner();
    let ptr = inner
        .extents
        .allocate(spec, heap, pages, ExtentInit::Uninit)
        .unwrap()
        .unwrap();
    let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
        panic!("expected extent owner");
    };
    assert!(heap.extent_inbox.queue(extent));

    assert_eq!(
        heap.alloc_extent(&mut inner, spec, ExtentInit::Uninit, &ctx),
        Err(HeapError::InvalidExtentPointer)
    );
    extent.claim(ptr).unwrap();
    assert_eq!(heap.flush(&mut inner, &ctx), Ok(()));
    drop(inner);
    assert_eq!(heaps.unbind(heap.id(), &ctx), Ok(()));
}

#[test]
fn reclaim_rejects_nonzero_leases() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    let lease = heap.state.acquire_lease(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    let inner = heap.lock_inner();
    assert!(!heap.reclaim(&inner, &heaps));
    drop(inner);
    assert!(heaps.get(id).is_some());
    drop(lease);
    let inner = heap.lock_inner();
    assert!(heap.reclaim(&inner, &heaps));
    drop(inner);
    assert!(heaps.get(id).is_none());
}
