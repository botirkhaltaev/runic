mod error;
pub(crate) mod extent;
mod heaps;
pub(crate) mod id;
pub(crate) mod inbox;
pub(crate) mod run;
mod state;
pub(crate) mod thread;

use core::ptr::NonNull;
use core::sync::atomic::AtomicU32;

use spin::Mutex;

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
pub(crate) use run::{Run, RunCache, RunError, RunHeap, RunId};
pub(crate) use state::HeapMode;
pub(crate) use thread::{THREAD_HEAP, ThreadFreeError};

/// Indexed heap entry: lifecycle, remote-free inboxes, and owner-local run/extent metadata.
///
/// Shared (`get`): atomics only — `enqueue`, mode queries.
/// Active exclusive metadata: [`ThreadHeap`](thread::ThreadHeap) via [`Heap::try_inner`].
/// Draining exclusive metadata: [`Heaps::{enqueue,free,flush}`](Heaps).
pub(crate) struct Heap {
    /// Lifecycle word — `pub(super)` so `Heaps` can close / wait / reactivate without a
    /// public `&HeapState` projection.
    pub(super) state: HeapState,
    run_inbox: RunInbox,
    extent_inbox: ExtentInbox,
    inner: Mutex<HeapInner>,
    /// Next Free heap index for [`Heaps`] (`u32::MAX` = end).
    pub(super) free_next: AtomicU32,
}

/// Exclusive run/extent metadata. Caller holds `MutexGuard<HeapInner>`.
pub(super) struct HeapInner {
    id: HeapId,
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
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            id,
            runs: RunHeap::new(),
            extents: ExtentHeap::new(config.extent()),
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

impl Heap {
    pub(crate) fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: HeapState::new(id.generation(), HeapMode::Active),
            run_inbox: RunInbox::new(),
            extent_inbox: ExtentInbox::new(),
            inner: Mutex::new(HeapInner::new(id, config)),
            free_next: AtomicU32::new(u32::MAX),
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

    pub(crate) fn mode(&self) -> HeapMode {
        self.state.mode()
    }

    pub(crate) fn leases(&self) -> u32 {
        self.state.leases()
    }

    pub(crate) fn close(&self, id: HeapId) -> Result<(), HeapError> {
        self.state.close(id)
    }

    pub(super) fn try_inner(&self) -> Option<spin::MutexGuard<'_, HeapInner>> {
        self.inner.try_lock()
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
    pub(super) fn reclaim(&self, inner: &HeapInner, heaps: &Heaps, index: u32) -> bool {
        let snap = self.state.load();
        if snap.retired || snap.mode != HeapMode::Draining || snap.leases != 0 {
            return false;
        }
        if inner.has_live() || !self.inboxes_empty() {
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
            heaps.push_free(self, index);
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

    /// Owner-local free (Inner only). Caller owns inbox `flush`.
    #[allow(clippy::unused_self)]
    pub(super) fn free(
        &self,
        inner: &mut HeapInner,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx<'_>,
    ) -> Result<(), HeapError> {
        match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap / inbox carry only live arena run pointers.
                if unsafe { run.as_ref() }.free(ptr).map_err(HeapError::from)? {
                    inner.runs.push_available(run)
                } else {
                    Ok(())
                }
            }
            PageOwner::Extent(extent) => inner.extents.free(extent, ptr, ctx.pages),
        }
    }

    /// Insert a run that just left full onto the available list. Not on the free hit.
    #[allow(clippy::unused_self)]
    pub(super) fn push_available(
        &self,
        inner: &mut HeapInner,
        run: NonNull<Run>,
    ) -> Result<(), HeapError> {
        inner.runs.push_available(run)
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
        inner.extents.allocate(spec, inner.id, ctx.pages, init)
    }

    /// Acquire a run without flushing the inbox (caller owns flush policy).
    #[allow(clippy::unused_self)]
    pub(super) fn acquire_run(
        &self,
        inner: &mut HeapInner,
        class: SizeClass,
        ctx: &AllocatorCtx<'_>,
    ) -> Option<NonNull<Run>> {
        inner.runs.acquire(class, inner.id, ctx.pages)
    }
}

#[cfg(test)]
mod tests;
