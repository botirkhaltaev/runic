use core::{
    hint,
    mem::size_of,
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::{
    config::AllocatorConfig,
    heap::HeapError,
    memory::{Mapping, OsMemory, PAGE_SIZE, PageOwner},
};

use super::state::HeapMode;
use super::{AllocatorCtx, Heap, HeapId, HeapInner};

const FREE_END: u32 = u32::MAX;
const CHUNK_BYTES: usize = 256 * 1024;
const MAX_CHUNKS: usize = 256;

/// Writer-owned chunk mappings and bump. Readers never touch this.
struct Chunks {
    mappings: [Option<Mapping>; MAX_CHUNKS],
    bump: u32,
}

/// Indexes heaps. Reader atomics (`chunks`, `len`) are lock-free; `grow` serializes insert.
pub(crate) struct Heaps {
    chunks: [AtomicPtr<Heap>; MAX_CHUNKS],
    /// One past the highest initialized index.
    len: AtomicU32,
    /// Intrusive Free-heap stack (`Heap::free_next`). Pushed from [`Heap::reclaim`].
    free_head: AtomicU32,
    config: AllocatorConfig,
    grow: Mutex<Chunks>,
}

// SAFETY: reader atomics publish immovable Heap slots; grow mutex serializes mapping
// ownership; config is immutable; free_head is atomic. Slots never move or unmap
// for the Heaps lifetime.
unsafe impl Send for Heaps {}
// SAFETY: same as Send — `get` returns `&Heap` from a published slot.
unsafe impl Sync for Heaps {}

impl Heaps {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            chunks: core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            len: AtomicU32::new(0),
            free_head: AtomicU32::new(FREE_END),
            config,
            grow: Mutex::new(Chunks {
                mappings: core::array::from_fn(|_| None),
                bump: 0,
            }),
        }
    }

    fn slots_per_chunk() -> u32 {
        let slot = size_of::<Heap>().max(1);
        debug_assert!(core::mem::align_of::<Heap>() <= PAGE_SIZE);
        let n = (CHUNK_BYTES / slot).max(1);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn slot(&self, index: u32) -> Option<&Heap> {
        if index >= self.len.load(Ordering::Acquire) {
            return None;
        }
        let per = Self::slots_per_chunk();
        let chunk_index = index / per;
        let offset = index % per;
        let chunk_i = usize::try_from(chunk_index).ok()?;
        let chunk = self.chunks.get(chunk_i)?.load(Ordering::Acquire);
        if chunk.is_null() {
            return None;
        }
        let offset = usize::try_from(offset).ok()?;
        // SAFETY: `len` was published after this slot was `ptr::write` and the chunk
        // pointer was stored. Occupied slots never move or unmap for the Heaps lifetime.
        Some(unsafe { &*chunk.add(offset) })
    }

    /// Acquire a heap for TLS bind: pop a Free heap or claim a fresh one.
    pub(crate) fn acquire(&self) -> Option<(HeapId, NonNull<Heap>)> {
        let mut grow = self.grow.lock();
        if let Some(acquired) = self.reuse() {
            return Some(acquired);
        }
        self.bump(&mut grow)
    }

    fn reuse(&self) -> Option<(HeapId, NonNull<Heap>)> {
        loop {
            let index = self.pop_free()?;
            let heap = self.slot(index)?;
            if heap.state.is_retired() || !heap.state.is_free() {
                continue;
            }

            let generation = heap.state.generation();
            let id = HeapId::new(index, generation)?;
            heap.reactivate(id);
            return Some((id, NonNull::from(heap)));
        }
    }

    fn pop_free(&self) -> Option<u32> {
        let mut index = self.free_head.load(Ordering::Acquire);
        loop {
            if index == FREE_END {
                return None;
            }
            let heap = self.slot(index)?;
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

    fn bump(&self, grow: &mut Chunks) -> Option<(HeapId, NonNull<Heap>)> {
        let index = grow.bump;
        if index == u32::MAX {
            return None;
        }
        let per = Self::slots_per_chunk();
        let chunk_index = index / per;
        let offset = index % per;
        let chunk_i = usize::try_from(chunk_index).ok()?;
        if chunk_i >= MAX_CHUNKS {
            return None;
        }

        if offset == 0 {
            let byte_len = usize::try_from(per).ok()?.checked_mul(size_of::<Heap>())?;
            let mapping = OsMemory::map(byte_len)?;
            let base = mapping.base().cast::<Heap>();
            *grow.mappings.get_mut(chunk_i)? = Some(mapping);
            if let Some(slot) = self.chunks.get(chunk_i) {
                slot.store(base.as_ptr(), Ordering::Release);
            }
        }

        let base = self.chunks.get(chunk_i)?.load(Ordering::Relaxed);
        if base.is_null() {
            return None;
        }
        let offset = usize::try_from(offset).ok()?;
        // SAFETY: this offset is inside the just-published (or already mapped) chunk
        // and has never been initialized.
        let slot = unsafe { NonNull::new_unchecked(base.add(offset)) };
        let generation = NonZeroU32::MIN;
        let id = HeapId::new(index, generation)?;
        // SAFETY: exclusive grow lock; slot is uninitialized mapped memory.
        unsafe { slot.as_ptr().write(Heap::new(id, self.config)) };
        grow.bump = index + 1;
        self.len.store(grow.bump, Ordering::Release);
        Some((id, slot))
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

    /// Generation-checked shared borrow. Lock-free directory read.
    pub(crate) fn get(&self, id: HeapId) -> Option<&Heap> {
        let heap = self.slot(id.index())?;
        heap.state.matches(id).then_some(heap)
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

    /// Late free while Draining. Then reclaim if this owner emptied.
    pub(crate) fn free(
        &self,
        id: HeapId,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), HeapError> {
        let (heap, mut inner) = self.admit(id)?;
        let emptied = inner.free(owner, ptr, ctx)?;
        if emptied {
            heap.reclaim(&inner, self, id.index());
        }
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

impl Drop for Heaps {
    fn drop(&mut self) {
        let n = self.len.load(Ordering::Relaxed);
        for index in 0..n {
            let Some(heap) = self.slot(index) else {
                continue;
            };
            // SAFETY: exclusive Heaps drop; each slot was `ptr::write` exactly once.
            unsafe { ptr::drop_in_place(ptr::from_ref(heap).cast_mut()) };
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
        let heap = heaps.get(id).unwrap();
        heap.state.store(max_gen, HeapMode::Draining, false, 0);
        let id_max = HeapId::new(index, max_gen).unwrap();
        assert_eq!(heaps.reclaim(id_max), Ok(()));
        assert!(heaps.get(id).is_none());
        assert!(heaps.get(id_max).is_none());
        assert!(heaps.slot(index).unwrap().state.is_retired());
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

    #[test]
    fn get_sees_published_heaps_across_chunks() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let n = usize::try_from(Heaps::slots_per_chunk()).unwrap() + 8;
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            let heaps = &heaps;
            scope.spawn(move || {
                for _ in 0..n {
                    let (id, _) = heaps.acquire().unwrap();
                    tx.send(id).unwrap();
                }
            });
            let mut seen = 0usize;
            for id in rx {
                while heaps.get(id).is_none() {
                    hint::spin_loop();
                }
                seen += 1;
            }
            assert_eq!(seen, n);
        });
    }
}
