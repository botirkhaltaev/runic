//! Heap lifecycle / lease tests. Active body + inbox reclaim live in `allocator` tests
//! (require `ThreadHeap::bind` / `Heaps::{enqueue,free,flush}`).

use super::*;
use crate::{config::AllocatorConfig, memory::PageMap};

use state::MAX_LEASES;

fn retire(heaps: &Heaps, id: HeapId) -> Result<(), HeapError> {
    let pages = PageMap::new();
    heaps.retire(
        id,
        &AllocatorCtx {
            pages: &pages,
            heaps,
        },
    )
}

#[test]
fn lease_rejected_after_close() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    assert!(heap.state.acquire_lease(id).is_err());
    assert_eq!(retire(&heaps, id), Ok(()));
}

#[test]
fn lease_count_overflow_fails_closed() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    // Forge the packed lease ceiling — acquiring `(1<<29)-1` real leases is not practical.
    heap.state
        .store(id.generation(), HeapMode::Active, false, MAX_LEASES);
    assert!(matches!(
        heap.state.acquire_lease(id),
        Err(HeapError::InvalidMetadata)
    ));
    heap.state
        .store(id.generation(), HeapMode::Active, false, 0);
    assert_eq!(retire(&heaps, id), Ok(()));
}

#[test]
fn adopt_promotes_draining_to_active() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.close(id), Ok(()));
    assert_eq!(heap.mode(), HeapMode::Draining);
    assert_eq!(heap.adopt(id), Ok(()));
    assert_eq!(heap.mode(), HeapMode::Active);
    assert_eq!(heap.adopt(id), Err(HeapError::InvalidHeap));
    assert_eq!(retire(&heaps, id), Ok(()));
}

#[test]
fn reclaim_rejects_nonzero_leases() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let id = heaps.acquire().unwrap().id();
    let heap = heaps.get(id).unwrap();
    let lease = heap.state.acquire_lease(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    {
        assert_eq!(heaps.reclaim(id), Ok(()));
    }
    assert!(heaps.get(id).is_some());
    drop(lease);
    {
        assert_eq!(heaps.reclaim(id), Ok(()));
    }
    assert!(heaps.get(id).is_none());
}
