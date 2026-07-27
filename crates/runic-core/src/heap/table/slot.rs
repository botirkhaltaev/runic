use core::{
    cell::UnsafeCell,
    hint,
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
};

use spin::Mutex;

use crate::{
    allocator::Allocator,
    arena::Arena,
    config::AllocatorConfig,
    heap::{ExtentHeapError, Heap, HeapId, HeapMode, RunHeapError},
    memory::{PageMap, PageOwner},
};

use super::inbox::{Inbox, RemoteList};

const MAX_HEAPS: usize = 64;
const MAX_HEAPS_U32: u32 = 64;
/// Max run/extent arena indices per heap (grow-on-demand; not pre-touched).
const HEAP_METADATA_CAPACITY: u32 = 16_384;

const MODE_SHIFT: u32 = 32;
const MODE_MASK: u64 = 0b11 << MODE_SHIFT;
const RETIRED_BIT: u64 = 1 << 34;
const PUBLISHER_SHIFT: u32 = 35;
const PUBLISHER_MASK: u64 = ((1u64 << 29) - 1) << PUBLISHER_SHIFT;
const MAX_PUBLISHERS: u32 = (1 << 29) - 1;

/// Decoded snapshot of the packed [`SlotState`] word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SlotStateSnapshot {
    generation: NonZeroU32,
    mode: HeapMode,
    retired: bool,
    publishers: u32,
}

/// Packed generation + mode + retired + publisher count — sole heap lifecycle authority.
///
/// Linearization / ordering:
/// - Active publish admit: successful `try_acquire_publisher` `AcqRel` CAS
/// - Inbox publish: head CAS in [`Inbox::push_batch`] (after lease admit)
/// - Active→Draining close: `close_active` `AcqRel` CAS (preserves publisher count)
/// - Publisher release: `Release` `fetch_sub`; retire observes zero with `Acquire` loads
/// - Free reactivation: `Release` store of Active after metadata rebind under the lifecycle lock
pub(crate) struct SlotState {
    word: AtomicU64,
}

impl SlotState {
    fn new(generation: NonZeroU32, mode: HeapMode) -> Self {
        Self {
            word: AtomicU64::new(Self::pack(generation, mode, false, 0)),
        }
    }

    fn pack(generation: NonZeroU32, mode: HeapMode, retired: bool, publishers: u32) -> u64 {
        debug_assert!(publishers <= MAX_PUBLISHERS);
        let mut word = u64::from(generation.get());
        word |= u64::from(mode.raw()) << MODE_SHIFT;
        if retired {
            word |= RETIRED_BIT;
        }
        word |= u64::from(publishers) << PUBLISHER_SHIFT;
        word
    }

    fn decode(word: u64) -> SlotStateSnapshot {
        let retired = word & RETIRED_BIT != 0;
        let generation = NonZeroU32::new(u32::try_from(word & 0xffff_ffff).unwrap_or(0))
            .unwrap_or(NonZeroU32::MIN);
        let mode = HeapMode::from_raw(u8::try_from((word & MODE_MASK) >> MODE_SHIFT).unwrap_or(0))
            .unwrap_or(HeapMode::Free);
        let publishers = u32::try_from((word & PUBLISHER_MASK) >> PUBLISHER_SHIFT).unwrap_or(0);
        SlotStateSnapshot {
            generation,
            mode,
            retired,
            publishers,
        }
    }

    fn load(&self) -> SlotStateSnapshot {
        Self::decode(self.word.load(Ordering::Acquire))
    }

    fn store(&self, generation: NonZeroU32, mode: HeapMode, retired: bool, publishers: u32) {
        self.word.store(
            Self::pack(generation, mode, retired, publishers),
            Ordering::Release,
        );
    }

    pub(crate) fn matches(&self, id: HeapId) -> bool {
        let snap = self.load();
        !snap.retired && snap.generation == id.generation()
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.load().mode
    }

    fn generation(&self) -> NonZeroU32 {
        self.load().generation
    }

    fn is_retired(&self) -> bool {
        self.load().retired
    }

    fn is_free(&self) -> bool {
        let snap = self.load();
        !snap.retired && snap.mode == HeapMode::Free && snap.publishers == 0
    }

    pub(crate) fn is_active(&self) -> bool {
        let snap = self.load();
        !snap.retired && snap.mode == HeapMode::Active
    }

    fn publishers(&self) -> u32 {
        self.load().publishers
    }

    /// Admit one Active publisher lease for `id`, or fail if closed / overflow.
    ///
    /// Counts in-flight Active **publish** admits only — not unpublished TLS batch contents
    /// (those stay live via `RemotePending` / `has_live_allocations`). Does not serialize
    /// concurrent freer bodies.
    #[inline]
    fn try_acquire_publisher(&self, id: HeapId) -> Result<(), HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.retired || snap.generation != id.generation() || snap.mode != HeapMode::Active {
                return Err(HeapError::InvalidHeap);
            }
            if snap.publishers == MAX_PUBLISHERS {
                return Err(HeapError::InvalidMetadata);
            }
            let next = Self::pack(snap.generation, snap.mode, false, snap.publishers + 1);
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    #[inline]
    fn release_publisher(&self) {
        let amount = 1u64 << PUBLISHER_SHIFT;
        let prev = self.word.fetch_sub(amount, Ordering::Release);
        // Underflow would corrupt mode/generation bits — fail closed.
        if (prev & PUBLISHER_MASK) < amount {
            Allocator::abort();
        }
    }

    /// Close Active admission while preserving the in-flight publisher count.
    fn close_active(&self, id: HeapId) -> Result<(), HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.retired || snap.generation != id.generation() {
                return Err(HeapError::InvalidHeap);
            }
            match snap.mode {
                HeapMode::Active => {
                    let next =
                        Self::pack(snap.generation, HeapMode::Draining, false, snap.publishers);
                    if self
                        .word
                        .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                HeapMode::Draining => return Ok(()),
                HeapMode::Free => return Err(HeapError::InvalidHeap),
            }
        }
    }

    /// Bump generation and set Free (publishers must already be zero), or permanently retire.
    fn bump_free_or_retire(&self) {
        let snap = self.load();
        debug_assert_eq!(snap.mode, HeapMode::Draining);
        debug_assert_eq!(snap.publishers, 0);
        match snap
            .generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU32::new)
        {
            Some(next) => self.store(next, HeapMode::Free, false, 0),
            None => self.store(snap.generation, HeapMode::Free, true, 0),
        }
    }
}

/// RAII Active publisher admission. Drop releases the packed count.
#[must_use]
pub(crate) struct PublisherLease<'a> {
    slot: &'a HeapSlot,
}

impl PublisherLease<'_> {
    pub(crate) fn publish(self, list: &RemoteList) {
        self.slot.inbox.push_batch(list);
    }
}

impl Drop for PublisherLease<'_> {
    fn drop(&mut self) {
        self.slot.state.release_publisher();
    }
}

/// Stable heap entity: lifecycle state, inbox, and owner-local metadata.
pub(crate) struct HeapSlot {
    state: SlotState,
    inbox: Inbox,
    heap: UnsafeCell<Heap>,
}

// SAFETY: state/inbox are atomic; heap mutation is exclusive TLS Active owner or directory-locked.
unsafe impl Send for HeapSlot {}
// SAFETY: shared readers use state/inbox atomics; heap is only mutated under the ownership rules above.
unsafe impl Sync for HeapSlot {}

impl HeapSlot {
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: SlotState::new(id.generation(), HeapMode::Active),
            inbox: Inbox::new(),
            heap: UnsafeCell::new(Heap::new(id, HEAP_METADATA_CAPACITY, config)),
        }
    }

    pub(crate) fn state(&self) -> &SlotState {
        &self.state
    }

    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    #[allow(clippy::mut_from_ref)]
    unsafe fn heap_mut(&self) -> &mut Heap {
        // SAFETY: same ownership contract as the method.
        unsafe { &mut *self.heap.get() }
    }

    fn reactivate(&self, id: HeapId) {
        // Rebind retained metadata first, then publish Active with Release.
        // SAFETY: Free reactivation runs under the directory lifecycle lock with publishers == 0.
        unsafe { self.heap_mut() }.rebind_heap_id(id);
        self.state
            .store(id.generation(), HeapMode::Active, false, 0);
    }

    pub(crate) fn publisher(&self, id: HeapId) -> Result<PublisherLease<'_>, HeapError> {
        self.state.try_acquire_publisher(id)?;
        Ok(PublisherLease { slot: self })
    }

    /// Mark Free and bump generation when Draining, empty, and publishers == 0.
    ///
    /// Publisher count tracks in-flight leases only; claimed-but-unpublished TLS batches keep
    /// the heap live via `has_live_allocations` until published and accepted.
    fn try_reclaim(&self) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.publishers != 0 {
            return false;
        }
        // SAFETY: directory lifecycle lock serializes Draining reclaim against heap mutation.
        if unsafe { (*self.heap.get()).has_live_allocations() } || !self.inbox.is_empty() {
            return false;
        }
        // Re-check route after live/inbox observation so a late publisher cannot be missed.
        let again = self.state.load();
        if again.generation != snap.generation
            || again.mode != HeapMode::Draining
            || again.publishers != 0
        {
            return false;
        }
        self.state.bump_free_or_retire();
        true
    }

    /// Drain inbox into this slot's heap (accept).
    ///
    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    pub(crate) unsafe fn flush(&self, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: same ownership contract as the method.
        let heap = unsafe { self.heap_mut() };
        while let Some(list) = self.inbox.drain() {
            for ptr in list {
                match pages.get(ptr) {
                    Some(PageOwner::Run(run)) => {
                        heap.runs.accept(run, ptr)?;
                    }
                    Some(PageOwner::Extent(extent)) => {
                        heap.extents.accept(extent, ptr, pages)?;
                    }
                    None => return Err(HeapError::InvalidPointer),
                }
            }
        }
        Ok(())
    }

    /// Flush inbox if needed, then owner-local free.
    ///
    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    pub(crate) unsafe fn free(
        &self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.inbox.is_empty() {
            // SAFETY: same ownership contract as the method.
            unsafe { self.flush(pages)? };
        }
        // SAFETY: same ownership contract as the method.
        unsafe { self.heap_mut() }.free(owner, ptr, pages)
    }

    /// Flush inbox if needed, then allocate a small block.
    ///
    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn alloc_run(
        &self,
        class: crate::size_class::SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            // SAFETY: Active TLS owner.
            unsafe { self.flush(pages) }.ok()?;
        }
        // SAFETY: Active TLS owner.
        unsafe { self.heap_mut() }.alloc_run(class, pages)
    }

    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn allocate_extent(
        &self,
        spec: crate::layout::LayoutSpec,
        pages: &PageMap,
        init: crate::heap::ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            // SAFETY: Active TLS owner.
            unsafe { self.flush(pages) }.ok()?;
        }
        // SAFETY: Active TLS owner.
        unsafe { self.heap_mut() }.allocate_extent(spec, pages, init)
    }

    /// Acquire a run without flushing the inbox (caller owns flush policy).
    ///
    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn acquire_run(
        &self,
        class: crate::size_class::SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<crate::heap::Run>> {
        // SAFETY: Active TLS owner.
        unsafe { self.heap_mut() }.acquire_run(class, pages)
    }

    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn return_available(
        &self,
        run: NonNull<crate::heap::Run>,
    ) -> Result<(), HeapError> {
        // SAFETY: Active TLS owner.
        unsafe { self.heap_mut() }
            .runs
            .return_available(run)
            .map_err(HeapError::from)
    }
}

struct HeapDirectoryState {
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
            if slot.state.is_retired() || !slot.state.is_free() {
                continue;
            }

            let generation = slot.state.generation();
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
        slot.state.matches(id).then_some(slot)
    }

    /// Publish a claimed remote-free batch to `id` (Active lease or Draining accept).
    pub(crate) fn publish(
        &self,
        id: HeapId,
        list: &RemoteList,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        let Some(slot) = self.slot(id) else {
            return Err(HeapError::InvalidHeap);
        };
        self.publish_on(slot, id, list, pages)
    }

    /// Publish on an already-resolved slot (Active lease or Draining accept).
    ///
    /// Same admit path as [`Self::publish`]; callers that already hold `slot` for `id`
    /// avoid a second directory lookup (unbound singleton / same-target capacity flush).
    pub(crate) fn publish_on(
        &self,
        slot: &HeapSlot,
        id: HeapId,
        list: &RemoteList,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        match slot.publisher(id) {
            Ok(lease) => {
                lease.publish(list);
                Ok(())
            }
            Err(HeapError::InvalidHeap) => self.publish_draining(id, list, pages),
            Err(error) => Err(error),
        }
    }

    #[cold]
    #[inline(never)]
    fn publish_draining(
        &self,
        id: HeapId,
        list: &RemoteList,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        let state = self.state.lock();
        let slot = Self::slot_locked(&state, id).ok_or(HeapError::InvalidHeap)?;
        if slot.state.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        slot.inbox.push_batch(list);
        // SAFETY: directory lifecycle lock held for Draining accept.
        unsafe { slot.flush(pages)? };
        let _ = slot.try_reclaim();
        Ok(())
    }

    /// Locked direct late free for a pointer not previously claimed into a remote batch.
    pub(crate) fn free_draining(
        &self,
        id: HeapId,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        let state = self.state.lock();
        let slot = Self::slot_locked(&state, id).ok_or(HeapError::InvalidHeap)?;
        if slot.state.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        // SAFETY: directory lifecycle lock held for Draining free.
        unsafe { slot.free(owner, ptr, pages)? };
        let _ = slot.try_reclaim();
        Ok(())
    }

    /// Owner thread gives up the slot: close Active, wait publishers, flush, reclaim.
    pub(crate) fn retire(&self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        {
            let state = self.state.lock();
            let slot = Self::slot_locked(&state, id).ok_or(HeapError::InvalidHeap)?;
            slot.state.close_active(id)?;
        }

        self.wait_publishers(id);

        let state = self.state.lock();
        let Some(slot) = Self::slot_locked(&state, id) else {
            // A concurrent Draining accept already reclaimed this generation.
            return Ok(());
        };
        if slot.state.mode() != HeapMode::Draining {
            return Ok(());
        }
        // SAFETY: directory lifecycle lock held after publishers drained.
        unsafe { slot.flush(pages)? };
        let _ = slot.try_reclaim();
        Ok(())
    }

    fn slot_locked(state: &HeapDirectoryState, id: HeapId) -> Option<&HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = state.slots.get(index)?;
        slot.state.matches(id).then_some(slot)
    }

    fn wait_publishers(&self, id: HeapId) {
        let mut spins = 0u32;
        loop {
            let Some(slot) = self.slot(id) else {
                return;
            };
            if slot.state.publishers() == 0 {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    InvalidHeap,
    InvalidPointer,
    DoubleFree,
    InvalidMetadata,
}

impl From<RunHeapError> for HeapError {
    fn from(error: RunHeapError) -> Self {
        match error {
            RunHeapError::InvalidPointer => Self::InvalidPointer,
            RunHeapError::DoubleFree => Self::DoubleFree,
            RunHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<crate::heap::RunError> for HeapError {
    fn from(error: crate::heap::RunError) -> Self {
        Self::from(RunHeapError::from(error))
    }
}

impl From<ExtentHeapError> for HeapError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent | ExtentHeapError::InvalidMetadata => {
                Self::InvalidMetadata
            }
            ExtentHeapError::InvalidPointer => Self::InvalidPointer,
            ExtentHeapError::DoubleFree => Self::DoubleFree,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AllocatorConfig;
    use std::sync::{Barrier, mpsc};
    use std::thread;

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
            slot.state.store(
                NonZeroU32::new(u32::MAX).unwrap(),
                HeapMode::Draining,
                false,
                0,
            );
            assert!(slot.try_reclaim());
            assert!(slot.state.is_retired());
        }
        assert!(directory.slot(id).is_none());
        let (other, _) = directory.acquire().unwrap();
        assert_ne!(other.index(), id.index());
    }

    #[test]
    fn publisher_rejected_after_close() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot.as_ref() };
        assert_eq!(slot.state.close_active(id), Ok(()));
        assert!(slot.publisher(id).is_err());
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
    }

    #[test]
    fn retire_waits_for_in_flight_publisher() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        let lease = slot.publisher(id).unwrap();
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
            while slot.state.mode() != HeapMode::Draining {
                hint::spin_loop();
            }
            assert_eq!(slot.state.publishers(), 1);
            assert!(done_rx.try_recv().is_err());
            drop(lease);
            done_rx.recv().unwrap();
        });

        assert!(directory.slot(id).is_none());
    }

    #[test]
    fn publisher_count_overflow_fails_closed() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        slot.state
            .store(id.generation(), HeapMode::Active, false, MAX_PUBLISHERS);
        assert!(matches!(
            slot.publisher(id),
            Err(HeapError::InvalidMetadata)
        ));
        slot.state
            .store(id.generation(), HeapMode::Active, false, 0);
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
    }

    #[test]
    fn concurrent_active_publishers_exact_once() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 64;
        #[repr(C)]
        struct Node {
            next: AtomicPtr<u8>,
        }

        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, _) = directory.acquire().unwrap();
        let mut nodes = Vec::with_capacity(THREADS * PER_THREAD);
        for _ in 0..(THREADS * PER_THREAD) {
            nodes.push(Node {
                next: AtomicPtr::new(ptr::null_mut()),
            });
        }
        let nodes = &nodes[..];
        let directory = &directory;

        thread::scope(|scope| {
            for t in 0..THREADS {
                scope.spawn(move || {
                    let start = t * PER_THREAD;
                    let end = start + PER_THREAD;
                    for node in &nodes[start..end] {
                        let ptr = NonNull::from(node).cast::<u8>();
                        let list = RemoteList::from_ends(ptr, ptr);
                        assert_eq!(directory.publish(id, &list, &PageMap::new()), Ok(()));
                    }
                });
            }
        });

        let slot = directory.slot(id).unwrap();
        assert_eq!(slot.state.publishers(), 0);
        let mut seen = 0usize;
        while let Some(list) = slot.inbox.drain() {
            for _ in list {
                seen += 1;
            }
        }
        assert_eq!(seen, THREADS * PER_THREAD);
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
    }

    #[test]
    fn try_reclaim_rejects_nonzero_publishers() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        assert_eq!(slot.state.close_active(id), Ok(()));
        assert!(slot.publisher(id).is_err());
        // Force a nonzero count under Draining via store for the reclaim gate.
        slot.state
            .store(id.generation(), HeapMode::Draining, false, 1);
        assert!(!slot.try_reclaim());
        slot.state
            .store(id.generation(), HeapMode::Draining, false, 0);
        assert!(slot.try_reclaim());
    }

    #[test]
    fn try_reclaim_rejects_nonempty_inbox() {
        #[repr(C)]
        struct Node {
            next: AtomicPtr<u8>,
        }

        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        assert_eq!(slot.state.close_active(id), Ok(()));
        let node = Node {
            next: AtomicPtr::new(ptr::null_mut()),
        };
        let ptr = NonNull::from(&node).cast::<u8>();
        slot.inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
        assert!(!slot.try_reclaim());
        assert!(slot.inbox.drain().is_some());
        assert!(slot.try_reclaim());
    }
}
