//! Heap lifecycle / lease tests. Active body + inbox reclaim live in `allocator` tests
//! (require `ThreadHeap::bind` / `LockedHeap`).

use super::*;
use crate::{config::AllocatorConfig, memory::PageMap};

use state::MAX_LEASES;

#[test]
fn lease_rejected_after_close() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let (id, _) = heaps.acquire().unwrap();
    let heap = heaps.get(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    assert!(heap.state.acquire_lease(id).is_err());
    assert_eq!(heaps.retire(id, &PageMap::new()), Ok(()));
}

#[test]
fn lease_count_overflow_fails_closed() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let (id, _) = heaps.acquire().unwrap();
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
    assert_eq!(heaps.retire(id, &PageMap::new()), Ok(()));
}

#[test]
fn reclaim_rejects_nonzero_leases() {
    let heaps = Heaps::new(AllocatorConfig::new());
    let (id, _) = heaps.acquire().unwrap();
    let heap = heaps.get(id).unwrap();
    let lease = heap.state.acquire_lease(id).unwrap();
    assert_eq!(heap.state.close(id), Ok(()));
    {
        let locked = heaps.lock(id).unwrap();
        drop(locked);
    }
    assert!(heaps.get(id).is_some());
    drop(lease);
    {
        let locked = heaps.lock(id).unwrap();
        drop(locked);
    }
    assert!(heaps.get(id).is_none());
}
