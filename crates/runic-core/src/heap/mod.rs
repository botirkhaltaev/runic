mod error;
pub(crate) mod extent;
mod heaps;
pub(crate) mod id;
pub(crate) mod inbox;
mod list;
mod queue;
pub(crate) mod run;
mod state;
pub(crate) mod thread;

use core::cell::Cell;
use core::num::NonZeroU32;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use list::Linked as ListLinked;
use queue::stack::Linked as StackLinked;

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
pub(crate) use thread::{THREAD_HEAPS, ThreadFreeError};

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
    /// Free-heap stack link. Concurrent with other `Heaps` pop/push.
    free: queue::stack::Link<Heap>,
    /// Owner-thread [`list::LinkedList`] membership. Remote threads do not read this.
    thread: list::Link<Heap>,
    /// Generation captured when this heap was linked onto a thread. Zero when unlinked.
    thread_gen: Cell<u32>,
}

impl ListLinked for Heap {
    fn links(&self) -> &list::Link<Self> {
        &self.thread
    }
}

impl StackLinked for Heap {
    fn links(&self) -> &queue::stack::Link<Self> {
        &self.free
    }
}

// SAFETY: arena slots are never moved. `thread` and `thread_gen` are written only by
// the thread that has this heap on its list, and that thread unlinks it before the
// slot can be bound again. Remote threads use `state`, the live counts, the inboxes,
// and `free`. `NonNull<Heap>` inside the thread link does not carry `Send` by itself.
unsafe impl Send for Heap {}
// SAFETY: remote threads share `&Heap` and only touch atomics: `state`, the live
// counts, the inboxes, and `free`. `thread` and `thread_gen` belong to the thread
// that has this heap on its list.
unsafe impl Sync for Heap {}

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

impl PageOwner {
    pub(crate) fn usable(self) -> usize {
        match self {
            Self::Run(run) => run.class().size(),
            Self::Extent(extent) => extent.len(),
        }
    }

    pub(crate) fn resize_in_place(
        self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, HeapError> {
        match self {
            Self::Run(run) => run.resize_in_place(ptr, spec).map_err(HeapError::from),
            Self::Extent(extent) => extent.resize_in_place(ptr, spec).map_err(HeapError::from),
        }
    }
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

    pub(super) fn release(&mut self, run: &'static Run, outcome: RunFree) -> Result<(), HeapError> {
        self.runs.release(run, outcome)
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
                let outcome = run.free(ptr).map_err(HeapError::from)?;
                self.runs.release(run, outcome)?;
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
            free: queue::stack::Link::new(),
            thread: list::Link::new(),
            thread_gen: Cell::new(0),
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
        let inner = self.inner.lock();
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

    /// Draining → Active. The metadata lock covers the lifecycle CAS only, so it
    /// still serializes adoption with Draining reclaim. The caller flushes after.
    #[cold]
    pub(crate) fn adopt(&self, id: HeapId) -> Result<(), HeapError> {
        if self.slot != id.slot() {
            return Err(HeapError::InvalidHeap);
        }
        let _inner = self.inner.lock();
        self.state.adopt(id)
    }

    /// Active exclusive. Fail → abort.
    pub(super) fn require_inner(&self) -> spin::MutexGuard<'_, HeapInner> {
        let Some(inner) = self.inner.try_lock() else {
            Allocator::abort();
        };
        inner
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

    /// Draining flush while the caller holds `inner`.
    ///
    /// [`Heaps::flush`](Heaps::flush) / [`Heaps::free`](Heaps::free) keep accept,
    /// publish, and reclaim one critical section. `owner` queues a claimed remote
    /// before accept so queue and flush share the guard.
    pub(super) fn flush(
        &self,
        inner: &mut HeapInner,
        ctx: &AllocatorCtx,
        owner: Option<PageOwner>,
    ) -> Result<(), HeapError> {
        if let Some(owner) = owner {
            debug_assert!(self == owner.heap());
            match owner {
                PageOwner::Run(run) => self.run_inbox.queue(run),
                PageOwner::Extent(extent) => self.extent_inbox.queue(extent),
            };
        }
        while !self.run_inbox.is_empty() {
            for run in self.run_inbox.drain() {
                let was_full = run.is_full();
                let accept = run.accept();
                let published = if was_full && !run.is_full() && !run.listed() {
                    inner.push_available(run)
                } else {
                    Ok(())
                };
                if accept == Accept::Requeue {
                    self.run_inbox.queue(run);
                }
                published?;
            }
        }
        while !self.extent_inbox.is_empty() {
            for extent in self.extent_inbox.drain() {
                extent.accept(extent.ptr())?;
                inner.extents.cache_or_unmap(extent, ctx.pages)?;
            }
        }
        Ok(())
    }

    /// Active owner flush. Accept each node outside the lock, then take a guard
    /// only to publish that node. Drop it before the next one.
    ///
    /// A miss flushes before mapping: claimed-full runs must be accepted first or
    /// `acquire` returns null.
    pub(super) fn flush_owner(&self, ctx: &AllocatorCtx) -> Result<(), HeapError> {
        while !self.run_inbox.is_empty() {
            for run in self.run_inbox.drain() {
                let was_full = run.is_full();
                let accept = run.accept();
                let published = if was_full && !run.is_full() && !run.listed() {
                    self.require_inner().push_available(run)
                } else {
                    Ok(())
                };
                if accept == Accept::Requeue {
                    self.run_inbox.queue(run);
                }
                published?;
            }
        }
        while !self.extent_inbox.is_empty() {
            for extent in self.extent_inbox.drain() {
                extent.accept(extent.ptr())?;
                self.require_inner()
                    .extents
                    .cache_or_unmap(extent, ctx.pages)?;
            }
        }
        Ok(())
    }

    /// Flush, then allocate one large block under a fresh guard.
    pub(super) fn alloc_extent(
        &'static self,
        spec: LayoutSpec,
        init: ExtentInit,
        ctx: &AllocatorCtx,
    ) -> Result<Option<NonNull<u8>>, HeapError> {
        self.flush_owner(ctx)?;
        self.require_inner()
            .extents
            .allocate(spec, self, ctx.pages, init)
    }
}

#[cfg(test)]
mod tests;
