mod error;
pub(crate) mod extent;
mod heaps;
pub(crate) mod id;
pub(crate) mod inbox;
pub(crate) mod run;
mod state;
pub(crate) mod thread;

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
/// Shared (`get`): atomics only — `enqueue`, mode queries.
/// Active body mutation: [`ThreadHeap`](thread::ThreadHeap) only.
/// Draining body mutation + reclaim: [`LockedHeap`] only.
pub(crate) struct Heap {
    /// Lifecycle word — `pub(super)` so `Heaps` can close / wait / reactivate without a
    /// public `&HeapState` projection.
    pub(super) state: HeapState,
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
    /// Already-queued claims coalesce with no lease. A new queue win takes a lease
    /// **before** `try_queue` so close cannot observe Queued without a link.
    pub(crate) fn enqueue(&self, id: HeapId, owner: PageOwner) -> Result<(), HeapError> {
        match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap / claim paths only pass live arena owners for this heap.
                let link = unsafe { run.as_ref() }.link();
                if link.is_queued() {
                    return Ok(());
                }
                let _lease = self.state.acquire_lease(id)?;
                if !link.try_queue() {
                    return Ok(());
                }
                self.link_owner(owner);
                Ok(())
            }
            PageOwner::Extent(extent) => {
                // SAFETY: PageMap / claim paths only pass live arena owners for this heap.
                let link = unsafe { extent.as_ref() }.link();
                if link.is_queued() {
                    return Ok(());
                }
                let _lease = self.state.acquire_lease(id)?;
                if !link.try_queue() {
                    return Ok(());
                }
                self.link_owner(owner);
                Ok(())
            }
        }
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

    pub(crate) fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.state.mode()
    }

    pub(crate) fn leases(&self) -> u32 {
        self.state.leases()
    }

    pub(crate) fn close(&self, id: HeapId) -> Result<(), HeapError> {
        self.state.close(id)
    }

    /// SAFETY: caller is the Active TLS owner (`ThreadHeap`) or holds [`LockedHeap`].
    #[allow(clippy::mut_from_ref)]
    unsafe fn body_mut(&self) -> &mut Body {
        // SAFETY: same ownership contract as the method.
        unsafe { &mut *self.body.get() }
    }

    pub(super) fn reactivate(&self, id: HeapId) {
        // Rebind retained metadata first, then publish Active with Release.
        // SAFETY: Free reactivation runs under the heaps arena lock with leases == 0.
        unsafe { self.body_mut() }.rebind(id);
        self.state
            .store(id.generation(), HeapMode::Active, false, 0);
    }

    /// Mark Free and bump generation when Draining, empty, and leases == 0.
    ///
    /// Only [`LockedHeap`] Drop may call this (holds `exclusive`).
    fn reclaim(&self) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.leases != 0 {
            return false;
        }
        // SAFETY: `LockedHeap` serializes Draining reclaim against body mutation.
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
    pub(super) unsafe fn flush(&self, pages: &PageMap) -> Result<(), HeapError> {
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
    pub(super) unsafe fn free(
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

    /// Flush inboxes if needed, then allocate one large block.
    ///
    /// SAFETY: caller is the Active TLS owner for this heap.
    pub(super) unsafe fn alloc_extent(
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
    pub(super) unsafe fn acquire_run(
        &self,
        class: SizeClass,
        pages: &PageMap,
    ) -> Option<NonNull<Run>> {
        // SAFETY: Active TLS owner.
        let body = unsafe { self.body_mut() };
        body.runs.acquire(class, body.id, pages)
    }

    /// SAFETY: caller is the Active TLS owner for this heap.
    pub(super) unsafe fn push_available(&self, run: NonNull<Run>) -> Result<(), HeapError> {
        // SAFETY: Active TLS owner.
        unsafe { self.body_mut() }.runs.push_available(run)
    }

    /// Take the Draining exclusive token. Caller must have observed Draining for `id`.
    pub(super) fn lock_exclusive(&self, id: HeapId) -> Result<LockedHeap<'_>, HeapError> {
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

/// Exclusive Draining access to one [`Heap`]. Drop attempts reclaim.
pub(crate) struct LockedHeap<'a> {
    heap: &'a Heap,
}

impl LockedHeap<'_> {
    /// Queue+link `owner` onto its inbox (Draining; no Active enqueue lease).
    pub(crate) fn enqueue(&mut self, owner: PageOwner) {
        match owner {
            PageOwner::Run(run) => {
                let _ = self.heap.run_inbox.push(run);
            }
            PageOwner::Extent(extent) => {
                let _ = self.heap.extent_inbox.push(extent);
            }
        }
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
mod tests;
