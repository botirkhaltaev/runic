use core::{
    hint,
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use spin::Mutex;

use crate::{arena::Arena, config::AllocatorConfig, heap::HeapError, memory::PageMap};

use super::state::HeapMode;
use super::{Heap, HeapId, LockedHeap};

const MAX_HEAPS: u32 = 64;
const MAX_HEAPS_LEN: usize = 64;
const _: () = assert!(MAX_HEAPS == 64 && MAX_HEAPS_LEN == 64);

/// Indexes and publishes heaps. Arena mutex covers claim/reuse only — never flush/accept.
pub(crate) struct Heaps {
    published: [AtomicPtr<Heap>; MAX_HEAPS_LEN],
    arena: Mutex<Arena<Heap>>,
    config: AllocatorConfig,
}

// SAFETY: published pointers are stable atomics; arena mutex serializes claim/reuse.
unsafe impl Send for Heaps {}
// SAFETY: Sync via atomics + Mutex; config is immutable after `new`.
unsafe impl Sync for Heaps {}

impl Heaps {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            published: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_HEAPS_LEN],
            arena: Mutex::new(Arena::new(MAX_HEAPS)),
            config,
        }
    }

    /// Acquire a heap for TLS bind: reuse a Free heap or claim a fresh one.
    pub(crate) fn acquire(&self) -> Option<(HeapId, NonNull<Heap>)> {
        let mut arena = self.arena.lock();
        if let Some(acquired) = Self::reuse(&mut arena) {
            return Some(acquired);
        }

        let index = arena.claim()?;
        let generation = NonZeroU32::MIN;
        let Some(id) = HeapId::new(index, generation) else {
            arena.release(index);
            return None;
        };
        let heap = Heap::new(id, self.config);

        if arena.insert(index, heap).is_none() {
            arena.release(index);
            return None;
        }

        let heap = NonNull::from(arena.get_mut(index)?);
        // SAFETY: Arena claim indices are always < MAX_HEAPS.
        self.published
            .get(usize::try_from(index).ok()?)?
            .store(heap.as_ptr(), Ordering::Release);
        Some((id, heap))
    }

    fn reuse(arena: &mut Arena<Heap>) -> Option<(HeapId, NonNull<Heap>)> {
        for index in 0..MAX_HEAPS {
            let Some(heap) = arena.get(index) else {
                continue;
            };
            if heap.state.is_retired() || !heap.state.is_free() {
                continue;
            }

            let generation = heap.state.generation();
            let id = HeapId::new(index, generation)?;
            heap.reactivate(id);
            return Some((id, NonNull::from(heap)));
        }

        None
    }

    /// Generation-checked shared borrow (lock-free via published pointers).
    pub(crate) fn get(&self, id: HeapId) -> Option<&Heap> {
        let ptr = self
            .published
            .get(usize::try_from(id.index()).ok()?)?
            .load(Ordering::Acquire);
        let heap = NonNull::new(ptr)?;
        // SAFETY: published pointers are set once on claim and never cleared; arena keeps storage.
        let heap = unsafe { heap.as_ref() };
        heap.state.matches(id).then_some(heap)
    }

    /// Exclusive Draining access to one heap (per-heap token; not the heaps arena mutex).
    pub(crate) fn lock(&self, id: HeapId) -> Result<LockedHeap<'_>, HeapError> {
        let heap = self.get(id).ok_or(HeapError::InvalidHeap)?;
        if heap.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        heap.lock_exclusive(id)
    }

    /// Owner thread gives up the heap: close Active, wait leases, flush, reclaim.
    pub(crate) fn retire(&self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        {
            let Some(heap) = self.get(id) else {
                // Already reclaimed / stale id — unbind races with LockedHeap Drop.
                return Ok(());
            };
            heap.close(id)?;
        }

        self.wait_leases(id);

        let mut locked = match self.lock(id) {
            Ok(locked) => locked,
            // A concurrent Draining accept already reclaimed this generation.
            Err(HeapError::InvalidHeap) => return Ok(()),
            Err(error) => return Err(error),
        };
        locked.flush(pages)?;
        Ok(())
    }

    fn wait_leases(&self, id: HeapId) {
        let mut spins = 0u32;
        loop {
            let Some(heap) = self.get(id) else {
                return;
            };
            if heap.leases() == 0 {
                return;
            }
            hint::spin_loop();
            spins = spins.saturating_add(1);
            if spins == 64 {
                spins = 0;
                std::thread::yield_now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Barrier, mpsc};
    use std::thread;

    use super::*;

    #[test]
    fn acquire_retire_reactivate_bumps_generation() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (first, _) = heaps.acquire().unwrap();
        assert_eq!(first.generation().get(), 1);
        assert_eq!(heaps.retire(first, &PageMap::new()), Ok(()));
        assert!(heaps.get(first).is_none());

        let (second, _) = heaps.acquire().unwrap();
        assert_eq!(second.index(), first.index());
        assert_eq!(second.generation().get(), 2);
        assert!(heaps.get(second).is_some());
        assert!(heaps.get(first).is_none());
    }

    #[test]
    fn stale_heap_id_rejected_after_reclaim() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, _) = heaps.acquire().unwrap();
        assert_eq!(heaps.retire(id, &PageMap::new()), Ok(()));
        assert!(heaps.get(id).is_none());
    }

    #[test]
    fn generation_exhaustion_permanently_retires_heap() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, _) = heaps.acquire().unwrap();
        let index = id.index();
        let max_gen = NonZeroU32::new(u32::MAX).unwrap();
        {
            let arena = heaps.arena.lock();
            let heap = arena.get(index).unwrap();
            // Drive route to terminal generation under Draining; LockedHeap Drop reclaims.
            heap.state.store(max_gen, HeapMode::Draining, false, 0);
        }
        let id_max = HeapId::new(index, max_gen).unwrap();
        {
            let locked = heaps.lock(id_max).unwrap();
            drop(locked);
        }
        assert!(heaps.get(id).is_none());
        assert!(heaps.get(id_max).is_none());
        {
            let arena = heaps.arena.lock();
            assert!(arena.get(index).unwrap().state.is_retired());
        }
        let (other, _) = heaps.acquire().unwrap();
        assert_ne!(other.index(), id.index());
    }

    #[test]
    fn retire_waits_for_in_flight_lease() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, _) = heaps.acquire().unwrap();
        let heap = heaps.get(id).unwrap();
        let lease = heap.state.acquire_lease(id).unwrap();
        let start = Barrier::new(2);
        let (done_tx, done_rx) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                assert_eq!(heaps.retire(id, &PageMap::new()), Ok(()));
                done_tx.send(()).unwrap();
            });

            start.wait();
            // Observe Draining with the lease still held — no wall-clock probe.
            while heap.state.mode() != HeapMode::Draining {
                hint::spin_loop();
            }
            assert_eq!(heap.state.leases(), 1);
            assert!(done_rx.try_recv().is_err());
            drop(lease);
            done_rx.recv().unwrap();
        });

        assert!(heaps.get(id).is_none());
    }
}
