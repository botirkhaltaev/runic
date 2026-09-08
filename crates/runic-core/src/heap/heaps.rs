use core::{
    hint,
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
};

use spin::RwLock;

use crate::{arena::Arena, config::AllocatorConfig, heap::HeapError, memory::PageOwner};

use super::state::HeapMode;
use super::{AllocatorCtx, Heap, HeapId, HeapInner};

const FREE_END: u32 = u32::MAX;

/// Indexes heaps. `RwLock` covers claim/reuse only — never flush/accept.
pub(crate) struct Heaps {
    arena: RwLock<Arena<Heap>>,
    /// Intrusive Free-heap stack (`Heap::free_next`). Pushed from [`Heap::reclaim`].
    free_head: AtomicU32,
    config: AllocatorConfig,
}

// SAFETY: `RwLock` serializes arena directory access; occupied slots never move; config is immutable.
unsafe impl Send for Heaps {}
// SAFETY: same as Send — `get` copies `&Heap` out of a read guard (immovable slots).
unsafe impl Sync for Heaps {}

impl Heaps {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            arena: RwLock::new(Arena::new()),
            free_head: AtomicU32::new(FREE_END),
            config,
        }
    }

    /// Acquire a heap for TLS bind: pop a Free heap or claim a fresh one.
    pub(crate) fn acquire(&self) -> Option<(HeapId, NonNull<Heap>)> {
        let mut arena = self.arena.write();
        if let Some(acquired) = self.reuse(&arena) {
            return Some(acquired);
        }

        let index = arena.vacant()?;
        let generation = NonZeroU32::MIN;
        let id = HeapId::new(index, generation)?;
        let heap = arena.insert(index, Heap::new(id, self.config))?;

        Some((id, NonNull::from(heap)))
    }

    fn reuse(&self, arena: &Arena<Heap>) -> Option<(HeapId, NonNull<Heap>)> {
        loop {
            let index = self.pop_free(arena)?;
            let heap = arena.get(index)?;
            if heap.state.is_retired() || !heap.state.is_free() {
                continue;
            }

            let generation = heap.state.generation();
            let id = HeapId::new(index, generation)?;
            heap.reactivate(id);
            return Some((id, NonNull::from(heap)));
        }
    }

    fn pop_free(&self, arena: &Arena<Heap>) -> Option<u32> {
        let mut index = self.free_head.load(Ordering::Acquire);
        loop {
            if index == FREE_END {
                return None;
            }
            let heap = arena.get(index)?;
            let next = heap.free_next.load(Ordering::Relaxed);
            match self.free_head.compare_exchange_weak(
                index,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(index),
                Err(current) => index = current,
            }
        }
    }

    /// Link a just-reclaimed Free heap. Caller holds Inner.
    pub(super) fn push_free(&self, heap: &Heap, index: u32) {
        let mut prev = self.free_head.load(Ordering::Relaxed);
        loop {
            heap.free_next.store(prev, Ordering::Relaxed);
            match self.free_head.compare_exchange_weak(
                prev,
                index,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => prev = current,
            }
        }
    }

    /// Generation-checked shared borrow. Read lock covers the index only.
    pub(crate) fn get(&self, id: HeapId) -> Option<&Heap> {
        let arena = self.arena.read();
        let heap = arena.get(id.index())?;
        if !heap.state.matches(id) {
            return None;
        }
        let heap = NonNull::from(heap);
        drop(arena);
        // SAFETY: occupied slots never move for the Arena lifetime (chunked append).
        // The read guard only indexes the directory; Heap bytes live in a stable mmap slot.
        Some(unsafe { heap.as_ref() })
    }

    /// Try to return a Draining heap to the Free list. No inbox accept.
    pub(crate) fn reclaim(&self, id: HeapId) -> Result<(), HeapError> {
        let (heap, inner) = self.admit(id)?;
        heap.reclaim(&inner, self, id.index());
        Ok(())
    }

    /// Inbox push while Draining (no Active lease). Then reclaim.
    pub(crate) fn enqueue(&self, id: HeapId, owner: PageOwner) -> Result<(), HeapError> {
        let (heap, inner) = self.admit(id)?;
        heap.drain_enqueue(owner);
        heap.reclaim(&inner, self, id.index());
        Ok(())
    }

    /// Late free while Draining. Then reclaim.
    pub(crate) fn free(
        &self,
        id: HeapId,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), HeapError> {
        let (heap, mut inner) = self.admit(id)?;
        heap.free(&mut inner, owner, ptr, ctx)?;
        heap.reclaim(&inner, self, id.index());
        Ok(())
    }

    /// Accept inboxes while Draining. Then reclaim.
    pub(crate) fn flush(&self, id: HeapId, ctx: &AllocatorCtx<'_>) -> Result<(), HeapError> {
        let (heap, mut inner) = self.admit(id)?;
        heap.flush(&mut inner, ctx)?;
        heap.reclaim(&inner, self, id.index());
        Ok(())
    }

    fn admit(&self, id: HeapId) -> Result<(&Heap, spin::MutexGuard<'_, HeapInner>), HeapError> {
        let heap = self.get(id).ok_or(HeapError::InvalidHeap)?;
        if heap.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        let inner = heap.lock_inner();
        if !heap.state.matches(id) || heap.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        Ok((heap, inner))
    }

    /// Owner thread gives up the heap: close Active, wait leases, reclaim, flush.
    pub(crate) fn retire(&self, id: HeapId, ctx: &AllocatorCtx<'_>) -> Result<(), HeapError> {
        {
            let Some(heap) = self.get(id) else {
                return Ok(());
            };
            heap.close(id)?;
        }

        self.wait_leases(id);

        match self.reclaim(id) {
            Ok(()) => {}
            Err(HeapError::InvalidHeap) => return Ok(()),
            Err(error) => return Err(error),
        }

        match self.flush(id, ctx) {
            Ok(()) | Err(HeapError::InvalidHeap) => Ok(()),
            Err(error) => Err(error),
        }
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
    use crate::memory::PageMap;

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
    fn acquire_retire_reactivate_bumps_generation() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (first, _) = heaps.acquire().unwrap();
        assert_eq!(first.generation().get(), 1);
        assert_eq!(retire(&heaps, first), Ok(()));
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
        assert_eq!(retire(&heaps, id), Ok(()));
        assert!(heaps.get(id).is_none());
    }

    #[test]
    fn generation_exhaustion_permanently_retires_heap() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, _) = heaps.acquire().unwrap();
        let index = id.index();
        let max_gen = NonZeroU32::new(u32::MAX).unwrap();
        {
            let arena = heaps.arena.write();
            let heap = arena.get(index).unwrap();
            // Drive route to terminal generation under Draining; flush reclaims.
            heap.state.store(max_gen, HeapMode::Draining, false, 0);
        }
        let id_max = HeapId::new(index, max_gen).unwrap();
        assert_eq!(heaps.reclaim(id_max), Ok(()));
        assert!(heaps.get(id).is_none());
        assert!(heaps.get(id_max).is_none());
        {
            let arena = heaps.arena.write();
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
                assert_eq!(retire(&heaps, id), Ok(()));
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

    #[test]
    fn acquire_grows_past_sixty_four_live_heaps() {
        const LIVE: usize = 96;
        let heaps = Heaps::new(AllocatorConfig::new());
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            let heaps = &heaps;
            for _ in 0..LIVE {
                let tx = tx.clone();
                scope.spawn(move || {
                    let (id, _) = heaps.acquire().unwrap();
                    tx.send(id).unwrap();
                });
            }
            drop(tx);
            let ids: Vec<_> = rx.iter().collect();
            assert_eq!(ids.len(), LIVE);

            let mut indexes: Vec<u32> = ids.iter().map(|id| id.index()).collect();
            indexes.sort_unstable();
            let unique = indexes.len();
            indexes.dedup();
            assert_eq!(indexes.len(), unique);

            for id in ids {
                assert_eq!(retire(heaps, id), Ok(()));
            }
        });

        let (reused, _) = heaps.acquire().unwrap();
        assert!(reused.index() < u32::try_from(LIVE).unwrap());
    }
}
