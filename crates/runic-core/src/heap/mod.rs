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
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use spin::Mutex;

use crate::{
    allocator::Allocator,
    config::AllocatorConfig,
    layout::LayoutSpec,
    memory::{PageMap, PageOwner},
    size_class::SizeClass,
};

use inbox::{Inbox, Node};
use state::HeapState;

pub(crate) use error::HeapError;
pub(crate) use extent::Extent;
pub(crate) use extent::ExtentInit;
pub(crate) use extent::heap::ExtentHeap;
pub(crate) use heaps::Heaps;
pub(crate) use id::HeapId;
pub(crate) use run::{Accept, Run, RunError, RunFree, RunHeap, RunId};
pub(crate) use state::HeapMode;
pub(crate) use thread::{THREAD_HEAPS, ThreadFreeError, ThreadHeaps};

/// Indexed heap entry: lifecycle, remote-free inboxes, and owner-local run/extent metadata.
///
/// Shared (`get`): atomics only — `id`, `active_id`, `enqueue`, mode, live counts.
/// Active exclusive metadata: [`ThreadHeaps`](thread::ThreadHeaps) via [`Heap::require_inner`]
/// (any TLS heap or the remote freer that [`Heap::adopt`]ed a Draining heap).
/// Draining exclusive metadata: [`Heaps::{free,flush}`](Heaps).
pub(crate) struct Heap {
    /// Lifecycle word — `pub(super)` so `Heaps` can close / wait / reactivate without a
    /// public `&HeapState` projection.
    pub(super) state: HeapState,
    /// Published arena slot (`HeapId` 1-based). Generation is in [`HeapState`].
    slot: NonZeroU32,
    /// Occupied runs. Updated on each run's 0↔1 live edge. Release store / Acquire load.
    runs_live: AtomicUsize,
    /// Occupied extents. Updated on allocate / cache-or-unmap. Release store / Acquire load.
    extents_live: AtomicUsize,
    run_inbox: Inbox<'static, Run>,
    extent_inbox: Inbox<'static, Extent>,
    inner: Mutex<HeapInner>,
    /// Next Free heap index for [`Heaps`] (`u32::MAX` = end).
    pub(super) free_next: AtomicU32,
}

impl PartialEq for Heap {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

impl Eq for Heap {}

/// Exclusive run/extent metadata. Caller holds `MutexGuard<HeapInner>`.
pub(super) struct HeapInner {
    runs: RunHeap,
    extents: ExtentHeap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OwnerState {
    Live,
    Empty,
}

/// Process `PageMap` + `Heaps` for miss / bind / unbind / Draining.
///
/// [`crate::allocator::Allocator::ctx`] is `'static`. Bind/init still require
/// that so TLS can store the heap. Other methods take a call-scoped borrow.
#[derive(Clone, Copy)]
pub(crate) struct AllocatorCtx<'a> {
    pub pages: &'a PageMap,
    pub heaps: &'a Heaps,
}

impl HeapInner {
    const fn new(config: AllocatorConfig) -> Self {
        Self {
            runs: RunHeap::new(config.run(), config.hints()),
            extents: ExtentHeap::new(config.extent(), config.hints()),
        }
    }

    pub(super) fn has_live(&self) -> bool {
        self.runs.has_live() || self.extents.has_live()
    }

    pub(super) fn push_available(&mut self, run: &'static Run) -> Result<(), HeapError> {
        self.runs.push_available(run)
    }

    pub(super) fn acquire_run(
        &mut self,
        class: SizeClass,
        pages: &PageMap,
        heap: &'static Heap,
    ) -> Option<&'static Run> {
        self.runs.acquire(class, heap, pages)
    }

    /// Owner-local free. `Empty` when this owner is no longer live.
    ///
    /// Caller owns inbox `flush`. A live owner means the heap is not reclaimable,
    /// so Draining `Heaps::free` can skip the arena scan.
    pub(super) fn free(
        &mut self,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<OwnerState, HeapError> {
        match owner {
            PageOwner::Run(run) => {
                if run.free(ptr).map_err(HeapError::from)? == RunFree::Available {
                    self.runs.push_available(run)?;
                }
                if run.is_discardable() {
                    run.discard();
                }
                Ok(if run.is_live() {
                    OwnerState::Live
                } else {
                    OwnerState::Empty
                })
            }
            PageOwner::Extent(extent) => {
                self.extents.free(extent, ptr, pages)?;
                Ok(OwnerState::Empty)
            }
        }
    }
}

impl Heap {
    pub(crate) const fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            state: HeapState::new(id.generation(), HeapMode::Active),
            slot: id.slot(),
            runs_live: AtomicUsize::new(0),
            extents_live: AtomicUsize::new(0),
            run_inbox: Inbox::new(),
            extent_inbox: Inbox::new(),
            inner: Mutex::new(HeapInner::new(config)),
            free_next: AtomicU32::new(u32::MAX),
        }
    }

    /// Arena slot plus the current generation.
    pub(crate) fn id(&self) -> HeapId {
        HeapId::from_slot(self.slot, self.state.generation())
    }

    pub(super) fn add_run_live(&self) {
        self.runs_live.fetch_add(1, Ordering::Release);
    }

    pub(super) fn sub_run_live(&self) {
        self.runs_live.fetch_sub(1, Ordering::Release);
    }

    pub(super) fn add_extent_live(&self) {
        self.extents_live.fetch_add(1, Ordering::Release);
    }

    pub(super) fn sub_extent_live(&self) {
        self.extents_live.fetch_sub(1, Ordering::Release);
    }

    /// Any occupied run or extent. Pairs with Release updates on the 0↔1 edges.
    pub(super) fn occupied(&self) -> bool {
        self.runs_live.load(Ordering::Acquire) != 0
            || self.extents_live.load(Ordering::Acquire) != 0
    }

    /// Push-or-coalesce `owner` onto its inbox. Active freers only.
    ///
    /// Already-queued claims coalesce with no lease. A new queue win takes a lease
    /// **before** `Inbox::queue` so close cannot observe Queued without a link.
    /// [`HeapState::acquire_lease`] is the Active admit; callers pass the `HeapId`
    /// captured from this heap.
    pub(crate) fn enqueue(&self, id: HeapId, owner: PageOwner) -> Result<(), HeapError> {
        debug_assert!(self == owner.heap());
        debug_assert_eq!(self.slot, id.slot());
        match owner {
            PageOwner::Run(run) => self.enqueue_node(id, &self.run_inbox, run),
            PageOwner::Extent(extent) => self.enqueue_node(id, &self.extent_inbox, extent),
        }
    }

    fn enqueue_node<T: Node + 'static>(
        &self,
        id: HeapId,
        inbox: &Inbox<'static, T>,
        node: &'static T,
    ) -> Result<(), HeapError> {
        if node.link().is_queued() {
            return Ok(());
        }
        let _lease = self.state.acquire_lease(id)?;
        inbox.queue(node);
        Ok(())
    }

    pub(super) fn inboxes_empty(&self) -> bool {
        self.run_inbox.is_empty() && self.extent_inbox.is_empty()
    }

    /// Current id when Active, from one Acquire load. Remote routing uses this
    /// instead of `id` then a second mode load.
    pub(crate) fn active_id(&self) -> Option<HeapId> {
        let snap = self.state.load();
        match snap.mode {
            HeapMode::Active => Some(HeapId::from_slot(self.slot, snap.generation)),
            HeapMode::Free | HeapMode::Draining | HeapMode::Retired => None,
        }
    }

    pub(crate) fn matches(&self, id: HeapId) -> bool {
        self.slot == id.slot() && self.state.matches(id)
    }

    /// Exclusive Inner while Draining for `id`. `owner` is the page when the
    /// caller already holds it (`Heaps::free` / claimed `flush`); `None` after `get`.
    pub(super) fn admit(
        &self,
        id: HeapId,
        owner: Option<PageOwner>,
    ) -> Result<spin::MutexGuard<'_, HeapInner>, HeapError> {
        if self.slot != id.slot() || self.mode() != HeapMode::Draining {
            return Err(HeapError::InvalidHeap);
        }
        if owner.is_some_and(|owner| self != owner.heap()) {
            return Err(HeapError::InvalidHeap);
        }
        let inner = self.lock_inner();
        let snap = self.state.load();
        if snap.mode != HeapMode::Draining || snap.generation != id.generation() {
            return Err(HeapError::InvalidHeap);
        }
        Ok(inner)
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.state.mode()
    }

    pub(crate) fn leases(&self) -> u32 {
        self.state.leases()
    }

    pub(crate) fn close(&self, id: HeapId) -> Result<(), HeapError> {
        if self.slot != id.slot() {
            return Err(HeapError::InvalidHeap);
        }
        self.state.close(id)
    }

    /// Draining → Active under the exclusive metadata lock.
    ///
    /// The winner keeps the returned guard for its first flush. Taking the lock
    /// before the lifecycle CAS serializes adoption with Draining reclaim.
    #[cold]
    pub(crate) fn adopt(&self, id: HeapId) -> Result<spin::MutexGuard<'_, HeapInner>, HeapError> {
        if self.slot != id.slot() {
            return Err(HeapError::InvalidHeap);
        }
        let inner = self.lock_inner();
        self.state.adopt(id)?;
        Ok(inner)
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

    pub(super) fn reactivate(&self) {
        self.state
            .store(self.state.generation(), HeapMode::Active, 0);
    }

    /// Mark Free and bump generation when Draining, empty, and leases == 0.
    pub(super) fn reclaim(&self, inner: &HeapInner, heaps: &Heaps) -> bool {
        let snap = self.state.load();
        if snap.mode != HeapMode::Draining || snap.leases != 0 {
            return false;
        }
        if !self.inboxes_empty() || self.occupied() || inner.has_live() {
            return false;
        }
        if !self.state.bump_or_retire(snap) {
            return false;
        }
        if !self.state.is_retired() {
            heaps.push_free(self);
        }
        true
    }

    /// Drain both inboxes into run/extent metadata (accept).
    ///
    /// `owner` queues a claimed remote before accept so queue+flush share Inner.
    pub(super) fn flush(
        &self,
        inner: &mut HeapInner,
        ctx: &AllocatorCtx,
        owner: Option<PageOwner>,
    ) -> Result<(), HeapError> {
        if let Some(owner) = owner {
            debug_assert!(self == owner.heap());
            match owner {
                PageOwner::Run(run) => {
                    self.run_inbox.queue(run);
                }
                PageOwner::Extent(extent) => {
                    self.extent_inbox.queue(extent);
                }
            }
        }
        while let Some(chain) = self.run_inbox.drain() {
            for run in chain {
                if inner.runs.accept(run)? == Accept::Requeue {
                    self.run_inbox.queue(run);
                }
            }
        }
        while let Some(chain) = self.extent_inbox.drain() {
            for extent in chain {
                inner.extents.accept(extent, extent.ptr(), ctx.pages)?;
            }
        }
        Ok(())
    }

    /// Flush inboxes if needed, then allocate one large block.
    pub(super) fn alloc_extent(
        &'static self,
        inner: &mut HeapInner,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        if !self.inboxes_empty() {
            self.flush(inner, ctx, None)?;
        }
        inner.extents.allocate(spec, self, ctx.pages, init)
    }
}

#[cfg(test)]
mod tests;
