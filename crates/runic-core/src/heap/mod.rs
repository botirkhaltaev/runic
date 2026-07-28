mod error;
pub(crate) mod extent;
mod heaps;
pub(crate) mod id;
pub(crate) mod inbox;
pub(crate) mod run;
mod state;
mod thread;

use core::{
    cell::UnsafeCell,
    hint,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    config::AllocatorConfig,
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::SizeClass,
};

use inbox::{ExtentInbox, InboxNode, RunInbox};
use state::HeapState;

pub(crate) use error::HeapError;
pub(crate) use extent::Extent;
pub(crate) use extent::heap::{ExtentHeap, ExtentInit};
pub(crate) use heaps::Heaps;
pub(crate) use id::HeapId;
pub(crate) use run::{Run, RunError, RunHeap, RunId};
pub(crate) use state::HeapMode;
pub(crate) use thread::{THREAD_HEAP, ThreadFreeError};

/// Max run/extent arena indices per heap (grow-on-demand; not pre-touched).
const HEAP_CAPACITY: u32 = 16_384;

/// Indexed heap entry: lifecycle, remote-free inboxes, and owner-local run/extent metadata.
///
/// Shared (`get`): atomics only — `state`, `enqueue`.
/// Active mutation: [`ThreadHeap`](thread::ThreadHeap) (TLS exclusivity).
/// Draining mutation: [`LockedHeap`].
pub(crate) struct Heap {
    state: HeapState,
    run_inbox: RunInbox,
    extent_inbox: ExtentInbox,
    body: UnsafeCell<Body>,
    /// Draining exclusive flag for [`LockedHeap`] — never touched on the Active hot path.
    exclusive: AtomicBool,
}

struct Body {
    id: HeapId,
    runs: RunHeap,
    extents: ExtentHeap,
}

impl Body {
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            id,
            runs: RunHeap::new(HEAP_CAPACITY),
            extents: ExtentHeap::new(HEAP_CAPACITY, config.extent()),
        }
    }

    fn rebind(&mut self, id: HeapId) {
        self.id = id;
        self.runs.rebind(id);
        self.extents.rebind(id);
    }

    fn has_live(&self) -> bool {
        self.runs.has_live() || self.extents.has_live()
    }
}

// SAFETY: state/inbox are atomic; body is mutated only by Active TLS (`ThreadHeap`) or
// `LockedHeap` (holds `exclusive`).
unsafe impl Send for Heap {}
// SAFETY: shared readers use state/inbox atomics; body mutation follows the rules above.
unsafe impl Sync for Heap {}

impl Heap {
    pub(crate) fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: HeapState::new(id.generation(), HeapMode::Active),
            run_inbox: RunInbox::new(),
            extent_inbox: ExtentInbox::new(),
            body: UnsafeCell::new(Body::new(id, config)),
            exclusive: AtomicBool::new(false),
        }
    }

    /// Push-or-coalesce `owner` onto its inbox. Active freers only.
    ///
    /// Coalesced claims (already queued) return `Ok` without taking an enqueue lease.
    /// Newly queued claims take a lease, then [`Inbox::link`]. `Err(InvalidHeap)` means the
    /// freer won the queue race but Active admission closed — caller must
    /// [`LockedHeap::enqueue`] under [`Heaps::lock`](Heaps::lock).
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
        let _lease = self.state.acquire_lease(id)?;
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

    pub(crate) fn state(&self) -> &HeapState {
        &self.state
    }

    /// SAFETY: caller is the Active TLS owner (`ThreadHeap`) or holds [`LockedHeap`].
    #[allow(clippy::mut_from_ref)]
    unsafe fn body_mut(&self) -> &mut Body {
        // SAFETY: same ownership contract as the method.
        unsafe { &mut *self.body.get() }
    }

    pub(crate) fn reactivate(&self, id: HeapId) {
        // Rebind retained metadata first, then publish Active with Release.
        // SAFETY: Free reactivation runs under the heaps arena lock with leases == 0.
        unsafe { self.body_mut() }.rebind(id);
        self.state
            .store(id.generation(), HeapMode::Active, false, 0);
    }

    /// Mark Free and bump generation when Draining, empty, and leases == 0.
    pub(crate) fn reclaim(&self) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.leases != 0 {
            return false;
        }
        // SAFETY: `LockedHeap` (or test) serializes Draining reclaim against body mutation.
        if unsafe { self.body_mut() }.has_live() || !self.inboxes_empty() {
            return false;
        }
        let again = self.state.load();
        if again.generation != snap.generation
            || again.mode != HeapMode::Draining
            || again.leases != 0
        {
            return false;
        }
        self.state.bump_or_retire();
        true
    }

    /// Drain both inboxes into run/extent metadata (accept).
    ///
    /// SAFETY: caller is the Active TLS owner or holds [`LockedHeap`].
    pub(crate) unsafe fn flush(&self, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: same ownership contract as the method.
        let body = unsafe { self.body_mut() };
        while let Some(chain) = self.run_inbox.drain() {
            for run in chain {
                // SAFETY: dequeued from this heap's run inbox; live arena run.
                if body.runs.accept(run)? {
                    let _ = self.run_inbox.push(run);
                }
            }
        }
        while let Some(chain) = self.extent_inbox.drain() {
            for extent in chain {
                // SAFETY: dequeued from this heap's extent inbox; live arena extent.
                let ptr = unsafe { extent.as_ref() }.ptr();
                body.extents.accept(extent, ptr, pages)?;
            }
        }
        Ok(())
    }

    /// Flush inboxes if needed, then owner-local free.
    ///
    /// SAFETY: caller is the Active TLS owner or holds [`LockedHeap`].
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
        let body = unsafe { self.body_mut() };
        match owner {
            PageOwner::Run(run) => body.runs.free(run, ptr),
            PageOwner::Extent(extent) => body.extents.free(extent, ptr, pages),
        }
    }

    /// Flush inboxes if needed, then allocate one small block.
    ///
    /// SAFETY: caller is the Active TLS owner for this heap.
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
            // SAFETY: Active TLS owner.
            let _ = unsafe { self.push_available(run) };
        }
        Some(ptr)
    }

    /// SAFETY: caller is the Active TLS owner for this heap.
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
        let body = unsafe { self.body_mut() };
        body.extents.allocate(spec, body.id, pages, init)
    }

    /// Acquire a run without flushing the inbox (caller owns flush policy).
    ///
    /// SAFETY: caller is the Active TLS owner for this heap.
    pub(crate) unsafe fn acquire_run(
        &self,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        // SAFETY: Active TLS owner.
        let body = unsafe { self.body_mut() };
        body.runs.acquire(class, body.id, pages)
    }

    /// SAFETY: caller is the Active TLS owner for this heap.
    pub(crate) unsafe fn push_available(&self, run: NonNull<Run>) -> Result<(), HeapError> {
        // SAFETY: Active TLS owner.
        unsafe { self.body_mut() }.runs.push_available(run)
    }

    /// Take the Draining exclusive token. Caller must have observed Draining for `id`.
    pub(crate) fn lock_exclusive(&self, id: HeapId) -> Result<LockedHeap<'_>, HeapError> {
        while self
            .exclusive
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
        if !self.state.matches(id) || self.state.mode() != HeapMode::Draining {
            self.exclusive.store(false, Ordering::Release);
            return Err(HeapError::InvalidHeap);
        }
        Ok(LockedHeap { heap: self })
    }
}

/// Exclusive Draining access to one [`Heap`]. Drop attempts [`Heap::reclaim`].
pub(crate) struct LockedHeap<'a> {
    heap: &'a Heap,
}

impl LockedHeap<'_> {
    /// Link an already-queued owner (no Active enqueue lease).
    pub(crate) fn enqueue(&mut self, owner: PageOwner) {
        self.heap.link_owner(owner);
    }

    /// Direct late free while exclusive.
    pub(crate) fn free(
        &mut self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), HeapError> {
        // SAFETY: exclusive Draining token held.
        unsafe { self.heap.free(owner, ptr, pages) }
    }

    pub(crate) fn flush(&mut self, pages: &PageMap) -> Result<(), HeapError> {
        // SAFETY: exclusive Draining token held.
        unsafe { self.heap.flush(pages) }
    }
}

impl Drop for LockedHeap<'_> {
    fn drop(&mut self) {
        let _ = self.heap.reclaim();
        self.heap.exclusive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use std::thread;

    use super::*;
    use crate::{
        config::AllocatorConfig, layout::LayoutSpec, memory::PageMap, size_class::SizeClasses,
    };

    use state::MAX_LEASES;

    #[test]
    fn lease_rejected_after_close() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap.as_ref() };
        assert_eq!(heap.state.close(id), Ok(()));
        assert!(heap.state.acquire_lease(id).is_err());
        assert_eq!(heaps.retire(id, &PageMap::new()), Ok(()));
    }

    #[test]
    fn lease_count_overflow_fails_closed() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap_ptr) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap_ptr.as_ref() };
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

    /// Many threads race `claim` + `enqueue` against blocks on one shared run.
    #[test]
    fn concurrent_active_leases_exact_once() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 64;

        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap_ptr) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap_ptr.as_ref() };
        let pages = PageMap::new();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();
        // SAFETY: test drives Active heap exclusively.
        let run = unsafe { heap.acquire_run(class, &pages) }.unwrap();
        let run_addr = run.as_ptr() as usize;
        let addrs: Vec<usize> = (0..THREADS * PER_THREAD)
            // SAFETY: run is a live arena run for this heap.
            .map(|_| unsafe { run.as_ref() }.allocate().unwrap().as_ptr() as usize)
            .collect();
        let addrs = &addrs[..];
        let pages = &pages;

        thread::scope(|scope| {
            for t in 0..THREADS {
                scope.spawn(move || {
                    // SAFETY: run_addr is `run`, a live arena run for this heap.
                    let run = NonNull::new(run_addr as *mut Run).unwrap();
                    let start = t * PER_THREAD;
                    for &addr in &addrs[start..start + PER_THREAD] {
                        // SAFETY: addr is a block owned by `run`, allocated above.
                        let ptr = NonNull::new(addr as *mut u8).unwrap();
                        unsafe { run.as_ref() }.claim(ptr).unwrap();
                        assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
                    }
                });
            }
        });

        assert_eq!(heap.state().leases(), 0);
        // SAFETY: test drives Active heap exclusively; producers have joined.
        unsafe { heap.flush(pages) }.unwrap();
        // SAFETY: same run pointer from this heap's live arena.
        assert!(!unsafe { run.as_ref() }.is_live());
        assert_eq!(heaps.retire(id, pages), Ok(()));
    }

    #[test]
    fn reclaim_rejects_nonzero_leases() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap_ptr) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap_ptr.as_ref() };
        assert_eq!(heap.state.close(id), Ok(()));
        assert!(heap.state.acquire_lease(id).is_err());
        heap.state
            .store(id.generation(), HeapMode::Draining, false, 1);
        assert!(!heap.reclaim());
        heap.state
            .store(id.generation(), HeapMode::Draining, false, 0);
        assert!(heap.reclaim());
    }

    #[test]
    fn reclaim_rejects_nonempty_run_inbox() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap_ptr) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap_ptr.as_ref() };
        let pages = PageMap::new();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();
        // SAFETY: test drives Active heap exclusively.
        let run = unsafe { heap.acquire_run(class, &pages) }.unwrap();
        assert_eq!(heap.state.close(id), Ok(()));

        assert!(heap.run_inbox.push(run));
        assert!(!heap.reclaim());
        // SAFETY: test drives Draining heap exclusively.
        unsafe { heap.flush(&pages) }.unwrap();
        assert!(heap.reclaim());
    }

    #[test]
    fn reclaim_rejects_nonempty_extent_inbox() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let (id, heap_ptr) = heaps.acquire().unwrap();
        // SAFETY: test-owned heap pointer from acquire.
        let heap = unsafe { heap_ptr.as_ref() };
        let pages = PageMap::new();
        let spec = LayoutSpec::from_layout(Layout::from_size_align(128 * 1024, 4096).unwrap());
        // SAFETY: test drives Active heap exclusively.
        let ptr = unsafe { heap.alloc_extent(spec, &pages, ExtentInit::Uninit) }.unwrap();
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        // SAFETY: extent is live and Allocated.
        unsafe { extent.as_ref() }.claim(ptr).unwrap();
        assert_eq!(heap.state.close(id), Ok(()));

        assert!(heap.extent_inbox.push(extent));
        assert!(!heap.reclaim());
        // SAFETY: test drives Draining heap exclusively.
        unsafe { heap.flush(&pages) }.unwrap();
        assert!(heap.reclaim());
    }
}
