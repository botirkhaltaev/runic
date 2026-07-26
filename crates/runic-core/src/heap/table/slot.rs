use core::{
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

use crate::{
    arena::Arena,
    config::AllocatorConfig,
    heap::{ExtentHeapError, Heap, HeapId, HeapMode, RunHeapError},
    memory::PageMap,
};

use super::inbox::{Inbox, RemoteList};

const MAX_HEAPS: usize = 64;
const MAX_HEAPS_U32: u32 = 64;
/// Max run/extent arena indices per heap (grow-on-demand; not pre-touched).
const HEAP_METADATA_CAPACITY: u32 = 16_384;

const MODE_SHIFT: u32 = 32;
const RETIRED_BIT: u64 = 1 << 40;

/// Packed generation + mode + retired flag — sole heap lifecycle authority.
pub(crate) struct HeapRoute {
    word: AtomicU64,
}

impl HeapRoute {
    fn new(generation: NonZeroU32, mode: HeapMode) -> Self {
        Self {
            word: AtomicU64::new(Self::pack(generation, mode, false)),
        }
    }

    fn pack(generation: NonZeroU32, mode: HeapMode, retired: bool) -> u64 {
        let mut word = u64::from(generation.get());
        word |= u64::from(mode.raw()) << MODE_SHIFT;
        if retired {
            word |= RETIRED_BIT;
        }
        word
    }

    fn load(&self) -> (NonZeroU32, HeapMode, bool) {
        let word = self.word.load(Ordering::Acquire);
        let retired = word & RETIRED_BIT != 0;
        let generation = NonZeroU32::new(u32::try_from(word & 0xffff_ffff).unwrap_or(0))
            .unwrap_or(NonZeroU32::MIN);
        let mode = HeapMode::from_raw(u8::try_from((word >> MODE_SHIFT) & 0xff).unwrap_or(0))
            .unwrap_or(HeapMode::Free);
        (generation, mode, retired)
    }

    fn store(&self, generation: NonZeroU32, mode: HeapMode, retired: bool) {
        self.word
            .store(Self::pack(generation, mode, retired), Ordering::Release);
    }

    pub(crate) fn matches(&self, id: HeapId) -> bool {
        let (generation, _, retired) = self.load();
        !retired && generation == id.generation()
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.load().1
    }

    fn generation(&self) -> NonZeroU32 {
        self.load().0
    }

    fn is_retired(&self) -> bool {
        self.load().2
    }

    fn is_free(&self) -> bool {
        let (_, mode, retired) = self.load();
        !retired && mode == HeapMode::Free
    }

    pub(crate) fn is_active(&self) -> bool {
        let (_, mode, retired) = self.load();
        !retired && mode == HeapMode::Active
    }

    fn set_mode(&self, mode: HeapMode) {
        let (generation, _, retired) = self.load();
        debug_assert!(!retired);
        self.store(generation, mode, false);
    }

    /// Bump generation and set Free, or permanently retire on overflow.
    fn bump_free_or_retire(&self) {
        let (generation, _, _) = self.load();
        match generation.get().checked_add(1).and_then(NonZeroU32::new) {
            Some(next) => self.store(next, HeapMode::Free, false),
            None => self.store(generation, HeapMode::Free, true),
        }
    }
}

/// Stable heap entity: route authority, inbox, publishers, owner metadata.
pub(crate) struct HeapSlot {
    route: HeapRoute,
    /// Reserved for PR5 producer leases; layout only in PR4 (always 0).
    #[allow(dead_code)]
    publishers: AtomicUsize,
    inbox: Inbox,
    heap: Heap,
}

// SAFETY: Slot mutation is coordinated by the directory mutex; inbox producers use atomics;
// owner-local heap metadata requires exclusive TLS Active or directory-locked Draining.
unsafe impl Send for HeapSlot {}
// SAFETY: same invariants as `Send`; shared access is inbox atomics or directory-locked.
unsafe impl Sync for HeapSlot {}

impl HeapSlot {
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            route: HeapRoute::new(id.generation(), HeapMode::Active),
            publishers: AtomicUsize::new(0),
            inbox: Inbox::new(),
            heap: Heap::new(id, HEAP_METADATA_CAPACITY, config),
        }
    }

    pub(crate) fn route(&self) -> &HeapRoute {
        &self.route
    }

    pub(crate) fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    pub(crate) fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    pub(crate) fn id(&self) -> HeapId {
        self.heap.id()
    }

    fn reactivate(&mut self, id: HeapId) {
        self.route.store(id.generation(), HeapMode::Active, false);
        self.heap.rebind_heap_id(id);
    }

    fn begin_drain(&self) {
        self.route.set_mode(HeapMode::Draining);
    }

    /// Mark Free and bump generation when empty; retire permanently on overflow.
    fn try_reclaim(&mut self) -> bool {
        if self.heap.has_live_allocations() || !self.inbox.is_empty() {
            return false;
        }
        self.route.bump_free_or_retire();
        true
    }

    /// Drain inbox into this slot's heap (accept).
    pub(crate) fn flush(&mut self, pages: &PageMap) -> Result<(), HeapError> {
        while let Some(list) = self.inbox.drain() {
            for ptr in list {
                match pages.get(ptr) {
                    Some(crate::memory::PageOwner::Run(run)) => {
                        self.heap.runs.accept(run, ptr)?;
                    }
                    Some(crate::memory::PageOwner::Extent(extent)) => {
                        self.heap.extents.accept(extent, ptr, pages)?;
                    }
                    None => return Err(HeapError::InvalidPointer),
                }
            }
        }
        Ok(())
    }

    /// Flush inbox if needed, then owner-local free.
    pub(crate) fn free(
        &mut self,
        owner: crate::memory::PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.inbox.is_empty() {
            self.flush(pages)?;
        }
        self.heap.free(owner, ptr, pages)
    }

    /// Flush inbox if needed, then allocate a small block.
    pub(crate) fn alloc_run(
        &mut self,
        class: crate::size_class::SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }
        self.heap.alloc_run(class, pages)
    }

    pub(crate) fn allocate_extent(
        &mut self,
        spec: crate::layout::LayoutSpec,
        pages: &PageMap,
        init: crate::heap::ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }
        self.heap.allocate_extent(spec, pages, init)
    }

    pub(crate) fn acquire_run(
        &mut self,
        class: crate::size_class::SizeClassId,
        pages: &PageMap,
    ) -> Option<NonNull<crate::heap::Run>> {
        if !self.inbox.is_empty() {
            self.flush(pages).ok()?;
        }
        self.heap.acquire_run(class, pages)
    }
}

/// Directory of stable [`HeapSlot`] addresses and table-locked lifecycle ops.
pub(crate) struct HeapDirectory {
    slots: Arena<HeapSlot>,
    published: [AtomicPtr<HeapSlot>; MAX_HEAPS],
    config: AllocatorConfig,
}

// SAFETY: HeapDirectory is owned under AllocatorInner's directory mutex.
unsafe impl Send for HeapDirectory {}

impl HeapDirectory {
    pub(crate) fn new(config: AllocatorConfig) -> Self {
        Self {
            slots: Arena::new(MAX_HEAPS_U32),
            published: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_HEAPS],
            config,
        }
    }

    /// Acquire a slot for TLS bind: reuse a Free slot or claim a fresh one.
    pub(crate) fn acquire(&mut self) -> Option<(HeapId, NonNull<HeapSlot>)> {
        if let Some(acquired) = self.acquire_reusable() {
            return Some(acquired);
        }

        let index = self.slots.claim()?;
        let generation = NonZeroU32::MIN;
        let Some(id) = HeapId::new(u32::try_from(index).ok()?, generation) else {
            self.slots.release(index);
            return None;
        };
        let slot = HeapSlot::new(id, self.config);

        if self.slots.insert(index, slot).is_none() {
            self.slots.release(index);
            return None;
        }

        let slot = NonNull::from(self.slots.get_mut(index)?);
        // SAFETY: Arena claim indices are always < MAX_HEAPS.
        unsafe { self.published.get_unchecked(index) }.store(slot.as_ptr(), Ordering::Release);
        // SAFETY: slot was just inserted into a live directory entry.
        Some((unsafe { slot.as_ref().id() }, slot))
    }

    fn acquire_reusable(&mut self) -> Option<(HeapId, NonNull<HeapSlot>)> {
        for index in 0..MAX_HEAPS {
            let Some(slot) = self.slots.get_mut(index) else {
                continue;
            };
            if slot.route.is_retired() || !slot.route.is_free() {
                continue;
            }

            let generation = slot.route.generation();
            let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
            slot.reactivate(id);
            let slot = NonNull::from(slot);
            // SAFETY: Free slot just reactivated in this directory.
            return Some((unsafe { slot.as_ref().id() }, slot));
        }

        None
    }

    /// Generation-checked shared borrow.
    pub(crate) fn slot(&self, id: HeapId) -> Option<&HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let ptr = self.published.get(index)?.load(Ordering::Acquire);
        let slot = NonNull::new(ptr)?;
        // SAFETY: published pointers are set once on claim and never cleared; arena keeps storage.
        let slot = unsafe { slot.as_ref() };
        slot.route.matches(id).then_some(slot)
    }

    /// Generation-checked exclusive borrow.
    pub(crate) fn slot_mut(&mut self, id: HeapId) -> Option<&mut HeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let ptr = self.published.get(index)?.load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }
        let slot = self.slots.get_mut(index)?;
        slot.route.matches(id).then_some(slot)
    }

    /// Publish a claimed remote-free batch to `id`.
    pub(crate) fn publish(
        &mut self,
        id: HeapId,
        list: &RemoteList,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        let mode = self.slot(id).ok_or(HeapError::InvalidHeap)?.route.mode();
        match mode {
            HeapMode::Active => {
                self.slot(id)
                    .ok_or(HeapError::InvalidHeap)?
                    .inbox()
                    .push_batch(list);
                Ok(())
            }
            HeapMode::Draining => {
                {
                    let slot = self.slot_mut(id).ok_or(HeapError::InvalidHeap)?;
                    slot.inbox().push_batch(list);
                    slot.flush(pages)?;
                }
                let _ = self.reclaim(id);
                Ok(())
            }
            HeapMode::Free => Err(HeapError::InvalidHeap),
        }
    }

    /// Owner thread gives up the slot: flush, enter Draining, flush again, reclaim if empty.
    pub(crate) fn retire(&mut self, id: HeapId, pages: &PageMap) -> Result<(), HeapError> {
        let slot = self.slot_mut(id).ok_or(HeapError::InvalidHeap)?;
        slot.flush(pages)?;
        slot.begin_drain();
        slot.flush(pages)?;
        let _ = slot.try_reclaim();
        Ok(())
    }

    /// If the slot is empty under Draining, mark Free (or retired) via route bump.
    pub(crate) fn reclaim(&mut self, id: HeapId) -> bool {
        let Some(slot) = self.slot_mut(id) else {
            return false;
        };
        slot.try_reclaim()
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

    #[test]
    fn acquire_retire_reactivate_bumps_generation() {
        let mut directory = HeapDirectory::new(AllocatorConfig::new());
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
        let mut directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, _) = directory.acquire().unwrap();
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
        assert!(directory.slot(id).is_none());
        assert!(directory.slot_mut(id).is_none());
        assert!(!directory.reclaim(id));
    }

    #[test]
    fn generation_exhaustion_permanently_retires_slot() {
        let mut directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, _) = directory.acquire().unwrap();
        let index = usize::try_from(id.index()).unwrap();
        let slot = directory.slots.get_mut(index).unwrap();
        // Drive route to terminal generation, then reclaim → retired.
        slot.route.store(
            NonZeroU32::new(u32::MAX).unwrap(),
            HeapMode::Draining,
            false,
        );
        assert!(slot.try_reclaim());
        assert!(slot.route.is_retired());
        assert!(directory.acquire_reusable().is_none());
        // Fresh claim can still allocate a different index.
        let (other, _) = directory.acquire().unwrap();
        assert_ne!(other.index(), id.index());
    }
}
