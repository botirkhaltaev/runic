use core::{cell::UnsafeCell, ptr::NonNull};

use spin::MutexGuard;

use crate::{
    config::AllocatorConfig,
    heap::{ExtentHeap, ExtentInit, HeapError, HeapId, Run, RunHeap},
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::SizeClass,
};

use super::{
    directory::HeapDirectoryState,
    inbox::{ExtentInbox, InboxNode, RunInbox},
    state::{HeapMode, SlotState},
};

/// Max run/extent arena indices per heap (grow-on-demand; not pre-touched).
const HEAP_METADATA_CAPACITY: u32 = 16_384;

/// Owner-local run/extent metadata for one `HeapId` (private to [`HeapSlot`]).
struct SlotHeap {
    id: HeapId,
    runs: RunHeap,
    extents: ExtentHeap,
}

impl SlotHeap {
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            id,
            runs: RunHeap::new(HEAP_METADATA_CAPACITY),
            extents: ExtentHeap::new(HEAP_METADATA_CAPACITY, config.extent()),
        }
    }

    fn rebind_heap_id(&mut self, id: HeapId) {
        self.id = id;
        self.runs.rebind_heap_id(id);
        self.extents.rebind_heap_id(id);
    }

    fn has_live(&self) -> bool {
        self.runs.has_live() || self.extents.has_live()
    }
}

/// Stable heap entity: lifecycle state, coalesced run/extent inboxes, and owner-local
/// run/extent metadata.
pub(crate) struct HeapSlot {
    state: SlotState,
    run_inbox: RunInbox,
    extent_inbox: ExtentInbox,
    heap: UnsafeCell<SlotHeap>,
}

// SAFETY: state/inbox are atomic; heap metadata is mutated only by the exclusive TLS Active
// owner or by directory-locked Draining paths.
unsafe impl Send for HeapSlot {}
// SAFETY: shared readers use state/inbox atomics; heap metadata is only mutated under the
// ownership rules above.
unsafe impl Sync for HeapSlot {}

impl HeapSlot {
    pub(super) fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: SlotState::new(id.generation(), HeapMode::Active),
            run_inbox: RunInbox::new(),
            extent_inbox: ExtentInbox::new(),
            heap: UnsafeCell::new(SlotHeap::new(id, config)),
        }
    }

    /// Push-or-coalesce `owner` onto its inbox. Active freers only.
    ///
    /// Coalesced claims (already queued) return `Ok` without taking a publisher lease.
    /// Newly queued claims take a lease, then [`Inbox::link`]. `Err(InvalidHeap)` means the
    /// freer won the queue race but Active admission closed — caller must
    /// [`LockedSlot::enqueue`] (link the already-queued node) under [`super::directory::HeapDirectory::lock`].
    pub(crate) fn enqueue(&self, id: HeapId, owner: PageOwner) -> Result<(), HeapError> {
        let queued = match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap / claim paths only pass live arena owners for this heap.
                unsafe { run.as_ref() }.link().try_queue()
            }
            PageOwner::Extent(extent) => {
                // SAFETY: PageMap / claim paths only pass live arena owners for this heap.
                unsafe { extent.as_ref() }.link().try_queue()
            }
        };
        if !queued {
            return Ok(());
        }
        let _lease = self.state.acquire_publisher(id)?;
        self.link_owner(owner);
        Ok(())
    }

    fn link_owner(&self, owner: PageOwner) {
        match owner {
            PageOwner::Run(run) => self.run_inbox.link(run),
            PageOwner::Extent(extent) => self.extent_inbox.link(extent),
        }
    }

    fn inboxes_empty(&self) -> bool {
        self.run_inbox.is_empty() && self.extent_inbox.is_empty()
    }

    pub(crate) fn state(&self) -> &SlotState {
        &self.state
    }

    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    #[allow(clippy::mut_from_ref)]
    unsafe fn heap_mut(&self) -> &mut SlotHeap {
        // SAFETY: same ownership contract as the method.
        unsafe { &mut *self.heap.get() }
    }

    pub(super) fn reactivate(&self, id: HeapId) {
        // Rebind retained metadata first, then publish Active with Release.
        // SAFETY: Free reactivation runs under the directory lifecycle lock with publishers == 0.
        unsafe { self.heap_mut() }.rebind_heap_id(id);
        self.state
            .store(id.generation(), HeapMode::Active, false, 0);
    }

    /// Mark Free and bump generation when Draining, empty, and publishers == 0.
    ///
    /// Publisher count tracks in-flight leases only; claimed-but-unpublished work keeps
    /// the heap live via `has_live` until enqueued and accepted.
    pub(super) fn try_reclaim(&self) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.publishers != 0 {
            return false;
        }
        // SAFETY: directory lifecycle lock serializes Draining reclaim against heap mutation.
        if unsafe { self.heap_mut() }.has_live() || !self.inboxes_empty() {
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

    /// Drain both inboxes into this slot's run/extent metadata (accept), re-pushing any run
    /// that still has a straggling claim after its scan (see `Run::accept`).
    ///
    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    pub(crate) unsafe fn flush(&self, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: same ownership contract as the method.
        let heap = unsafe { self.heap_mut() };
        while let Some(chain) = self.run_inbox.drain() {
            for run in chain {
                // SAFETY: dequeued from this slot's run inbox; the pointer is a live run
                // published from this heap's arena for as long as it may be claimed.
                if heap.runs.accept(run)? {
                    let _ = self.run_inbox.push(run);
                }
            }
        }
        while let Some(chain) = self.extent_inbox.drain() {
            for extent in chain {
                // SAFETY: dequeued from this slot's extent inbox; the pointer is a live
                // extent published from this heap's arena for as long as it may be claimed.
                let ptr = unsafe { extent.as_ref() }.ptr();
                heap.extents.accept(extent, ptr, pages)?;
            }
        }
        Ok(())
    }

    /// Flush inboxes if needed, then owner-local free.
    ///
    /// SAFETY: caller is the Active TLS owner or holds the directory lifecycle lock.
    pub(crate) unsafe fn free(
        &self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        if !self.inboxes_empty() {
            // SAFETY: same ownership contract as the method.
            unsafe { self.flush(pages)? };
        }
        // SAFETY: same ownership contract as the method.
        let heap = unsafe { self.heap_mut() };
        match owner {
            PageOwner::Run(run) => heap.runs.free(run, ptr),
            PageOwner::Extent(extent) => heap.extents.free(extent, ptr, pages),
        }
    }

    /// Flush inboxes if needed, then allocate one small block: acquire a run, take a block,
    /// and return the run to the available list if it is not full.
    ///
    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn alloc_run(
        &self,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if !self.inboxes_empty() {
            // SAFETY: Active TLS owner.
            unsafe { self.flush(pages) }.ok()?;
        }
        // SAFETY: Active TLS owner.
        let run = unsafe { self.acquire_run(class, pages) }?;
        // SAFETY: run was just returned by this heap's live arena.
        let ptr = unsafe { run.as_ref() }.allocate()?;
        // SAFETY: same run pointer from this heap's live arena.
        if !unsafe { run.as_ref() }.is_full() {
            // SAFETY: Active TLS owner returning a run acquired from this slot.
            let _ = unsafe { self.return_available(run) };
        }
        Some(ptr)
    }

    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn alloc_extent(
        &self,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        if !self.inboxes_empty() {
            // SAFETY: Active TLS owner.
            unsafe { self.flush(pages) }.ok()?;
        }
        // SAFETY: Active TLS owner.
        let heap = unsafe { self.heap_mut() };
        heap.extents.allocate(spec, heap.id, pages, init)
    }

    /// Acquire a run without flushing the inbox (caller owns flush policy).
    ///
    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn acquire_run(
        &self,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        // SAFETY: Active TLS owner.
        let heap = unsafe { self.heap_mut() };
        heap.runs.acquire(class, heap.id, pages)
    }

    /// SAFETY: caller is the Active TLS owner for this slot.
    pub(crate) unsafe fn return_available(&self, run: NonNull<Run>) -> Result<(), HeapError> {
        // SAFETY: Active TLS owner.
        unsafe { self.heap_mut() }.runs.return_available(run)
    }
}

/// Exclusive directory-locked access to a Draining slot (lifecycle mutex held).
///
/// Drop attempts [`HeapSlot::try_reclaim`] under the same lock.
pub(crate) struct LockedSlot<'a> {
    slot: NonNull<HeapSlot>,
    _guard: MutexGuard<'a, HeapDirectoryState>,
}

impl<'a> LockedSlot<'a> {
    pub(super) fn new(slot: NonNull<HeapSlot>, guard: MutexGuard<'a, HeapDirectoryState>) -> Self {
        Self {
            slot,
            _guard: guard,
        }
    }

    fn slot(&self) -> &HeapSlot {
        // SAFETY: `_guard` keeps the directory arena alive and serializes mutation; `slot`
        // was resolved under that lock for a matching Draining generation.
        unsafe { self.slot.as_ref() }
    }

    /// Link an already-queued owner (no Active publisher lease). Caller won
    /// [`super::inbox::InboxLink::try_queue`] on the Active path that then observed closed
    /// admission.
    pub(crate) fn enqueue(&mut self, owner: PageOwner) {
        self.slot().link_owner(owner);
    }

    /// Direct late free while exclusive (heap winding down; freer did not claim).
    pub(crate) fn free(
        &mut self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: directory lifecycle lock held for Draining free.
        unsafe { self.slot().free(owner, ptr, pages) }
    }

    pub(crate) fn flush(&mut self, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: directory lifecycle lock held for Draining accept.
        unsafe { self.slot().flush(pages) }
    }
}

impl Drop for LockedSlot<'_> {
    fn drop(&mut self) {
        let _ = self.slot().try_reclaim();
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use std::thread;

    use super::*;
    use crate::{
        config::AllocatorConfig,
        heap::{ExtentInit, HeapDirectory, Run},
        layout::LayoutSpec,
        memory::PageMap,
        size_class::SizeClasses,
    };

    use super::super::state::MAX_PUBLISHERS;

    #[test]
    fn publisher_rejected_after_close() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot.as_ref() };
        assert_eq!(slot.state.close_active(id), Ok(()));
        assert!(slot.state.acquire_publisher(id).is_err());
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
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
            slot.state.acquire_publisher(id),
            Err(HeapError::InvalidMetadata)
        ));
        slot.state
            .store(id.generation(), HeapMode::Active, false, 0);
        assert_eq!(directory.retire(id, &PageMap::new()), Ok(()));
    }

    /// Many threads race `claim` + `enqueue` against blocks on one shared run.
    /// Inbox coalescing collapses concurrent queue wins into at most one live inbox entry
    /// at a time, but every publisher lease must still be acquired and released exactly
    /// once, and every claimed block must end up accepted after a flush (no lost wakeup,
    /// no leaked lease).
    #[test]
    fn concurrent_active_publishers_exact_once() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 64;

        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        let pages = PageMap::new();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();
        // SAFETY: test drives Active slot exclusively.
        let run = unsafe { slot.acquire_run(class, &pages) }.unwrap();
        // Addresses, not `NonNull`: raw pointers are not `Send`/`Sync`, and `run`/`ptrs`
        // only ever cross the thread boundary below as plain integers.
        let run_addr = run.as_ptr() as usize;
        let addrs: Vec<usize> = (0..THREADS * PER_THREAD)
            // SAFETY: run is a live arena run for this slot.
            .map(|_| unsafe { run.as_ref() }.allocate().unwrap().as_ptr() as usize)
            .collect();
        let addrs = &addrs[..];
        let pages = &pages;

        thread::scope(|scope| {
            for t in 0..THREADS {
                scope.spawn(move || {
                    // SAFETY: run_addr is `run`, a live arena run for this slot.
                    let run = NonNull::new(run_addr as *mut Run).unwrap();
                    let start = t * PER_THREAD;
                    for &addr in &addrs[start..start + PER_THREAD] {
                        // SAFETY: addr is a block owned by `run`, allocated above.
                        let ptr = NonNull::new(addr as *mut u8).unwrap();
                        unsafe { run.as_ref() }.claim(ptr).unwrap();
                        assert_eq!(slot.enqueue(id, PageOwner::Run(run)), Ok(()));
                    }
                });
            }
        });

        assert_eq!(slot.state().publishers(), 0);
        // SAFETY: test drives Active slot exclusively; producers have joined.
        unsafe { slot.flush(pages) }.unwrap();
        // SAFETY: same run pointer from this slot's live arena.
        assert!(!unsafe { run.as_ref() }.is_live());
        assert_eq!(directory.retire(id, pages), Ok(()));
    }

    #[test]
    fn try_reclaim_rejects_nonzero_publishers() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        assert_eq!(slot.state.close_active(id), Ok(()));
        assert!(slot.state.acquire_publisher(id).is_err());
        // Force a nonzero count under Draining via store for the reclaim gate.
        slot.state
            .store(id.generation(), HeapMode::Draining, false, 1);
        assert!(!slot.try_reclaim());
        slot.state
            .store(id.generation(), HeapMode::Draining, false, 0);
        assert!(slot.try_reclaim());
    }

    #[test]
    fn try_reclaim_rejects_nonempty_run_inbox() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        let pages = PageMap::new();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();
        // SAFETY: test drives Active slot exclusively.
        let run = unsafe { slot.acquire_run(class, &pages) }.unwrap();
        assert_eq!(slot.state.close_active(id), Ok(()));

        assert!(slot.run_inbox.push(run));
        assert!(!slot.try_reclaim());
        // SAFETY: test drives Draining slot exclusively.
        unsafe { slot.flush(&pages) }.unwrap();
        assert!(slot.try_reclaim());
    }

    #[test]
    fn try_reclaim_rejects_nonempty_extent_inbox() {
        let directory = HeapDirectory::new(AllocatorConfig::new());
        let (id, slot_ptr) = directory.acquire().unwrap();
        // SAFETY: test-owned slot pointer from acquire.
        let slot = unsafe { slot_ptr.as_ref() };
        let pages = PageMap::new();
        let spec = LayoutSpec::from_layout(Layout::from_size_align(128 * 1024, 4096).unwrap());
        // SAFETY: test drives Active slot exclusively.
        let ptr = unsafe { slot.alloc_extent(spec, &pages, ExtentInit::Uninit) }.unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        // SAFETY: extent is live and Allocated.
        unsafe { extent.as_ref() }.claim(ptr).unwrap();
        assert_eq!(slot.state.close_active(id), Ok(()));

        assert!(slot.extent_inbox.push(extent));
        assert!(!slot.try_reclaim());
        // SAFETY: test drives Draining slot exclusively.
        unsafe { slot.flush(&pages) }.unwrap();
        assert!(slot.try_reclaim());
    }
}
