mod error;
pub(crate) mod extent;
mod heaps;
pub(crate) mod id;
pub(crate) mod inbox;
pub(crate) mod run;
mod state;
pub(crate) mod thread;

use core::num::NonZeroU32;
use core::ptr::NonNull;
use core::sync::atomic::AtomicU32;

use spin::Mutex;

use crate::{
    allocator::Allocator,
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
pub(crate) use thread::{THREAD_HEAP, ThreadFreeError, ThreadHeap};

/// Indexed heap entry: lifecycle, remote-free inboxes, and owner-local run/extent metadata.
///
/// Shared (`get`): atomics only — `id`, `enqueue`, mode queries.
/// Active exclusive metadata: [`ThreadHeap`](thread::ThreadHeap) via [`Heap::require_inner`]
/// (bound owner or the remote freer that [`Heap::adopt`]ed a Draining heap).
/// Draining exclusive metadata: [`Heaps::{enqueue,free,flush}`](Heaps).
pub(crate) struct Heap {
    /// Lifecycle word — `pub(super)` so `Heaps` can close / wait / reactivate without a
    /// public `&HeapState` projection.
    pub(super) state: HeapState,
    /// Published arena slot (`HeapId` 1-based). Generation is in [`HeapState`].
    slot: NonZeroU32,
    run_inbox: RunInbox,
    extent_inbox: ExtentInbox,
    inner: Mutex<HeapInner>,
    /// Next Free heap index for [`Heaps`] (`u32::MAX` = end).
    pub(super) free_next: AtomicU32,
}

/// Exclusive run/extent metadata. Caller holds `MutexGuard<HeapInner>`.
pub(super) struct HeapInner {
    runs: RunHeap,
    extents: ExtentHeap,
}

/// Parent bag passed into heap children (`PageMap` + `Heaps`).
#[derive(Clone, Copy)]
pub(crate) struct AllocatorCtx<'a> {
    pub pages: &'a PageMap,
    pub heaps: &'a Heaps,
}

impl HeapInner {
    fn new(config: AllocatorConfig) -> Self {
        Self {
            runs: RunHeap::new(config.run()),
            extents: ExtentHeap::new(config.extent()),
        }
    }

    fn rebind(&mut self, id: HeapId) {
        self.runs.rebind(id);
        self.extents.rebind(id);
    }

    pub(super) fn has_live(&self) -> bool {
        self.runs.has_live() || self.extents.has_live()
    }

    /// Owner-local free. `Ok(true)` when this owner is no longer live.
    ///
    /// Caller owns inbox `flush`. A live owner means the heap is not reclaimable,
    /// so Draining `Heaps::free` can skip the arena scan.
    pub(super) fn free(
        &mut self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<bool, HeapError> {
        match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap / inbox carry only live arena run pointers.
                if unsafe { run.as_ref() }.free(ptr).map_err(HeapError::from)? {
                    self.runs.push_available(run)?;
                }
                // SAFETY: same live arena run; `is_live` counts allocated and claimed.
                Ok(!unsafe { run.as_ref() }.is_live())
            }
            PageOwner::Extent(extent) => {
                self.extents.free(extent, ptr, ctx.pages)?;
                Ok(true)
            }
        }
    }

    pub(super) fn push_available(&mut self, run: NonNull<Run>) -> Result<(), HeapError> {
        self.runs.push_available(run)
    }

    pub(super) fn acquire_run(
        &mut self,
        class: SizeClass,
        pages: &PageMap,
        heap: &Heap,
    ) -> Option<NonNull<Run>> {
        self.runs.acquire(class, heap.id(), Some(heap), pages)
    }
}

impl Heap {
    pub(crate) fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: HeapState::new(id.generation(), HeapMode::Active),
            slot: id.slot(),
            run_inbox: RunInbox::new(),
            extent_inbox: ExtentInbox::new(),
            inner: Mutex::new(HeapInner::new(config)),
            free_next: AtomicU32::new(u32::MAX),
        }
    }

    /// Arena slot plus the current generation.
    pub(crate) fn id(&self) -> HeapId {
        HeapId::from_slot(self.slot, self.state.generation())
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

    /// Inbox push without an Active lease. Draining only.
    pub(super) fn drain_enqueue(&self, owner: PageOwner) {
        match owner {
            PageOwner::Run(run) => {
                let _ = self.run_inbox.push(run);
            }
            PageOwner::Extent(extent) => {
                let _ = self.extent_inbox.push(extent);
            }
        }
    }

    pub(super) fn inboxes_empty(&self) -> bool {
        self.run_inbox.is_empty() && self.extent_inbox.is_empty()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub(crate) fn matches(&self, id: HeapId) -> bool {
        self.state.matches(id)
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

    /// Draining → Active. First remote freer wins; loser sees Active.
    #[cold]
    pub(crate) fn adopt(&self, id: HeapId) -> Result<(), HeapError> {
        self.state.adopt(id)
    }

    pub(super) fn try_inner(&self) -> Option<spin::MutexGuard<'_, HeapInner>> {
        self.inner.try_lock()
    }

    /// Active exclusive. Fail → abort.
    pub(super) fn require_inner(&self) -> spin::MutexGuard<'_, HeapInner> {
        let Some(inner) = self.try_inner() else {
            Allocator::abort();
        };
        inner
    }

    pub(super) fn lock_inner(&self) -> spin::MutexGuard<'_, HeapInner> {
        self.inner.lock()
    }

    pub(super) fn reactivate(&self, id: HeapId) {
        self.lock_inner().rebind(id);
        self.state
            .store(id.generation(), HeapMode::Active, false, 0);
    }

    /// Mark Free and bump generation when Draining, empty, and leases == 0.
    pub(super) fn reclaim(&self, inner: &HeapInner, heaps: &Heaps) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.leases != 0 {
            return false;
        }
        if !self.inboxes_empty() || inner.has_live() {
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
        if !self.state.is_retired() {
            heaps.push_free(self, self.id().index());
        }
        true
    }

    /// Drain both inboxes into run/extent metadata (accept).
    pub(super) fn flush(
        &self,
        inner: &mut HeapInner,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), HeapError> {
        while let Some(chain) = self.run_inbox.drain() {
            for run in chain {
                // SAFETY: dequeued from this heap's run inbox; live arena run.
                if inner.runs.accept(run)? {
                    let _ = self.run_inbox.push(run);
                }
            }
        }
        while let Some(chain) = self.extent_inbox.drain() {
            for extent in chain {
                // SAFETY: dequeued from this heap's extent inbox; live arena extent.
                let ptr = unsafe { extent.as_ref() }.ptr();
                inner.extents.accept(extent, ptr, ctx.pages)?;
            }
        }
        Ok(())
    }

    /// Flush inboxes if needed, then allocate one large block.
    pub(super) fn alloc_extent(
        &self,
        inner: &mut HeapInner,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx<'_>,
    ) -> Option<NonNull<u8>> {
        if !self.inboxes_empty() {
            self.flush(inner, ctx).ok()?;
        }
        inner.extents.allocate(spec, self.id(), ctx.pages, init)
    }
}

#[cfg(test)]
mod tests;
