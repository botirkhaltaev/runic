use core::{
    hint,
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use spin::Mutex;

use crate::{
    arena::Arena,
    config::AllocatorConfig,
    heap::{HeapError, HeapId},
    memory::PageMap,
};

use super::{
    slot::{HeapSlot, LockedSlot},
    state::HeapMode,
};

const MAX_HEAPS: usize = 64;
const MAX_HEAPS_U32: u32 = 64;

pub(super) struct HeapDirectoryState {
    slots: Arena<HeapSlot>,
    config: AllocatorConfig,
}

/// Directory facade: lock-free published slot lookup; lifecycle ops lock private state.
pub(crate) struct HeapDirectory {
    published: [AtomicPtr<HeapSlot>; MAX_HEAPS],
    state: Mutex<HeapDirectoryState>,
}

// SAFETY: published pointers are stable atomics; state mutex serializes arena mutation.
unsafe impl Send for HeapDirectory {}
// SAFETY: Sync via atomics + Mutex.
unsafe impl Sync for HeapDirectory {}

impl HeapDirectory {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            published: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_HEAPS],
            state: Mutex::new(HeapDirectoryState {
                slots: Arena::new(MAX_HEAPS_U32),
                config,
            }),
        }
    }

    /// Acquire a slot for TLS bind: reuse a Free slot or claim a fresh one.
    pub(crate) fn acquire(&self) -> Option<(HeapId, NonNull<HeapSlot>)> {
        let mut state = self.state.lock();
        if let Some(acquired) = Self::acquire_reusable(&mut state) {
            return Some(acquired);
        }

        let index = state.slots.claim()?;
        let generation = NonZeroU32::MIN;
        let Some(id) = HeapId::new(u32::try_from(index).ok()?, generation) else {
            state.slots.release(index);
            return None;
        };
        let slot = HeapSlot::new(id, state.config);

        if state.slots.insert(index, slot).is_none() {
            state.slots.release(index);
            return None;
        }

        let slot = NonNull::from(state.slots.get_mut(index)?);
        // SAFETY: Arena claim indices are always < MAX_HEAPS.
        unsafe { self.published.get_unchecked(index) }.store(slot.as_ptr(), Ordering::Release);
        Some((id, slot))
    }

    fn acquire_reusable(state: &mut HeapDirectoryState) -> Option<(HeapId, NonNull<HeapSlot>)> {
        for index in 0..MAX_HEAPS {
            let Some(slot) = state.slots.get(index) else {
                continue;
            };
            if slot.state().is_retired() || !slot.state().is_free() {
                continue;
            }

            let generation = slot.state().generation();
            let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
            slot.reactivate(id);
            return Some((id, NonNull::from(slot)));
        }

        None
    }

    /// Generation-checked shared borrow (lock-free via published pointers).
    pub(crate) fn slot(&self, id: HeapId) -> Option<&HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let ptr = self.published.get(index)?.load(Ordering::Acquire);
        let slot = NonNull::new(ptr)?;
        // SAFETY: published pointers are set once on claim and never cleared; arena keeps storage.
        let slot = unsafe { slot.as_ref() };
        slot.state().matches(id).then_some(slot)
    }

    /// Lifecycle mutex + generation check; only while mode is Draining.
    pub(crate) fn lock(&self, id: HeapId) -> Result<LockedSlot<'_>, HeapError> {
        let guard = self.state.lock();
        let slot = Self::slot_locked(&guard, id).ok_or(HeapError::InvalidHeap)?;
        if slot.state().mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        Ok(LockedSlot::new(NonNull::from(slot), guard))
    }

    /// Owner thread gives up the slot: close Active, wait publishers, flush, reclaim.
    pub(crate) fn retire(&self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        {
            let state = self.state.lock();
            let slot = Self::slot_locked(&state, id).ok_or(HeapError::InvalidHeap)?;
            slot.state().close_active(id)?;
        }

        self.wait_publishers(id);

        let mut locked = match self.lock(id) {
            Ok(locked) => locked,
            // A concurrent Draining accept already reclaimed this generation.
            Err(HeapError::InvalidHeap) => return Ok(()),
            Err(error) => return Err(error),
        };
        locked.flush(pages)?;
        Ok(())
    }

    fn slot_locked(state: &HeapDirectoryState, id: HeapId) -> Option<&HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = state.slots.get(index)?;
        slot.state().matches(id).then_some(slot)
    }

    fn wait_publishers(&self, id: HeapId) {
        let mut spins = 0u32;
        loop {
            let Some(slot) = self.slot(id) else {
                return;
            };
            if slot.state().publishers() == 0 {
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
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (first, _) = directory.acquire().unwrap();
        assert_eq!(first.generation().get(), 1);
        assert_eq!(directory.retire(first, &PageMap::new()), Ok(()));
        assert!(directory.slot(first).is_none());

        let (second, _) = directory.acquire().unwrap();
        assert_eq!(second.index(), first.index());
        assert_eq!(second.generation().get(), 2);
        assert!(directory.slot(second).is_some());
        assert!(directory.slot(first).is_none());
    }

    #[test]
    fn stale_heap_id_rejected_after_reclaim() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, _) = directory.acquire().unwrap();
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
        assert!(directory.slot(id).is_none());
    }

    #[test]
    fn generation_exhaustion_permanently_retires_slot() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, _) = directory.acquire().unwrap();
        let index = usize::try_from(id.index()).unwrap();
        {
            let state = directory.state.lock();
            let slot = state.slots.get(index).unwrap();
            // Drive route to terminal generation under Draining, then reclaim → retired.
            slot.state().store(
                NonZeroU32::new(u32::MAX).unwrap(),
                HeapMode::Draining,
                false,
                0,
            );
            assert!(slot.try_reclaim());
            assert!(slot.state().is_retired());
        }
        assert!(directory.slot(id).is_none());
        let (other, _) = directory.acquire().unwrap();
        assert_ne!(other.index(), id.index());
    }

    #[test]
    fn retire_waits_for_in_flight_publisher() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        let lease = slot.state().acquire_publisher(id).unwrap();
        let start = Barrier::new(2);
        let (done_tx, done_rx) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
                done_tx.send(()).unwrap();
            });

            start.wait();
            // Observe Draining with the lease still held — no wall-clock probe.
            while slot.state().mode() != HeapMode::Draining {
                hint::spin_loop();
            }
            assert_eq!(slot.state().publishers(), 1);
            assert!(done_rx.try_recv().is_err());
            drop(lease);
            done_rx.recv().unwrap();
        });

        assert!(directory.slot(id).is_none());
    }
}
