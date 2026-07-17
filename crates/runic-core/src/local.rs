use core::{
    cell::Cell,
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::{
    allocation::Allocation,
    allocator::{AllocatorCore, AllocatorState},
    config::{AllocatorConfig, RunPolicy},
    extent::{Extent, ExtentHeap, ExtentHeapError},
    layout::LayoutSpec,
    memory::PageMap,
    ownership::{HeapId, HeapOwner},
    run::{Run, RunHeap, RunHeapError},
    size_class::SizeClassId,
    slot_store::{SlotStore, SlotStoreError},
};

const MAX_LOCAL_HEAPS: usize = 32;
const MAX_LOCAL_HEAPS_U32: u32 = 32;
const REMOTE_INBOX_CAPACITY: usize = 16;
const LOCAL_METADATA_CAPACITY: u32 = 1024;
const THREAD_HEAP_CAPACITY: usize = 8;

pub(crate) struct LocalHeapTable {
    slots: SlotStore<LocalHeapSlot>,
    generations: [NonZeroU32; MAX_LOCAL_HEAPS],
    config: AllocatorConfig,
}

// SAFETY: LocalHeapTable is owned by AllocatorState. Slot mutation and remote routing are
// coordinated by the allocator lock; local heap internals use their own locks for owner-thread
// fast paths.
unsafe impl Send for LocalHeapTable {}

impl LocalHeapTable {
    pub(crate) const fn new(config: AllocatorConfig) -> Self {
        Self {
            slots: SlotStore::new(MAX_LOCAL_HEAPS_U32),
            generations: [NonZeroU32::MIN; MAX_LOCAL_HEAPS],
            config,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<LocalHeapHandle> {
        let index = self.slots.reserve()?;
        let generation = *self.generations.get(index)?;
        let id = HeapId::new(u32::try_from(index).ok()?, generation)?;
        let slot = LocalHeapSlot {
            generation,
            heap: LocalHeap::new(id, self.config),
        };

        if self.slots.insert(index, slot).is_err() {
            let _ = self.slots.release(index);
            return None;
        }

        let heap = NonNull::from(&mut self.slot_mut(id)?.heap);
        Some(LocalHeapHandle::new(id, heap))
    }

    pub(crate) fn get_mut(&mut self, id: HeapId) -> Option<&mut LocalHeap> {
        Some(&mut self.slot_mut(id)?.heap)
    }

    pub(crate) fn abandon(
        &mut self,
        id: HeapId,
        pages: &mut PageMap,
    ) -> Result<(), LocalHeapError> {
        let heap = self.get_mut(id).ok_or(LocalHeapError::InvalidHeap)?;
        heap.drain(pages)?;
        heap.abandon();
        Ok(())
    }

    pub(crate) fn reclaim(&mut self, id: HeapId) -> Result<(), LocalHeapError> {
        let index = usize::try_from(id.index()).map_err(|_| LocalHeapError::InvalidHeap)?;
        let heap = self.get_mut(id).ok_or(LocalHeapError::InvalidHeap)?;

        if !heap.is_abandoned() || heap.has_live_allocations() {
            return Ok(());
        }

        let _ = self
            .slots
            .remove(index)
            .ok_or(LocalHeapError::InvalidHeap)?;
        let generation = self
            .generations
            .get_mut(index)
            .ok_or(LocalHeapError::InvalidHeap)?;
        *generation = NonZeroU32::new(generation.get().wrapping_add(1)).unwrap_or(NonZeroU32::MIN);

        Ok(())
    }

    fn slot_mut(&mut self, id: HeapId) -> Option<&mut LocalHeapSlot> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = self.slots.get_mut(index)?;
        (slot.generation == id.generation()).then_some(slot)
    }
}

struct LocalHeapSlot {
    generation: NonZeroU32,
    heap: LocalHeap,
}

pub(crate) struct LocalHeap {
    id: HeapId,
    abandoned: AtomicBool,
    runs: Mutex<RunHeap>,
    extents: Mutex<ExtentHeap>,
    inbox: Mutex<RemoteInbox>,
    remote_pending: AtomicBool,
    live: AtomicU32,
}

// SAFETY: LocalHeap uses interior synchronization for mutable heap state that can be reached from
// remote frees. The owner thread may use the run heap fast path while shared routing uses the inbox.
unsafe impl Sync for LocalHeap {}

impl LocalHeap {
    fn new(id: HeapId, config: AllocatorConfig) -> Self {
        Self {
            id,
            abandoned: AtomicBool::new(false),
            runs: Mutex::new(RunHeap::new(
                LOCAL_METADATA_CAPACITY,
                config.with_run_policy(RunPolicy::DropEmpty).run(),
            )),
            extents: Mutex::new(ExtentHeap::new(LOCAL_METADATA_CAPACITY, config.extent())),
            inbox: Mutex::new(RemoteInbox::new()),
            remote_pending: AtomicBool::new(false),
            live: AtomicU32::new(0),
        }
    }

    pub(crate) fn allocate_run(&self, class: SizeClassId) -> Option<Allocation> {
        let allocation = self.runs.lock().allocate_available(class)?;
        self.retain_allocation();
        Some(allocation)
    }

    pub(crate) fn allocate_run_slow(
        &self,
        class: SizeClassId,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let allocation = self
            .runs
            .lock()
            .allocate(class, HeapOwner::Local(self.id), pages)?;
        self.retain_allocation();
        Some(allocation)
    }

    pub(crate) fn allocate_extent(
        &self,
        spec: LayoutSpec,
        pages: &mut PageMap,
    ) -> Option<Allocation> {
        let allocation = self
            .extents
            .lock()
            .allocate(spec, HeapOwner::Local(self.id), pages)?;
        self.retain_allocation();
        Some(allocation)
    }

    pub(crate) fn free_run(
        &self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), LocalHeapError> {
        self.drain_if_pending(pages)?;
        self.runs.lock().free(run, ptr, pages)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn free_extent(
        &self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &mut PageMap,
    ) -> Result<(), LocalHeapError> {
        self.drain_if_pending(pages)?;
        self.extents.lock().free(extent, ptr, pages)?;
        self.release_allocation();
        Ok(())
    }

    pub(crate) fn enqueue(
        &self,
        free: RemoteFree,
        pages: &mut PageMap,
    ) -> Result<(), LocalHeapError> {
        match self.inbox.lock().enqueue(free) {
            Ok(()) => {
                self.remote_pending.store(true, Ordering::Release);
                Ok(())
            }
            Err(RemoteError::Duplicate) => Err(LocalHeapError::DoubleFree),
            Err(RemoteError::Full) => {
                self.drain(pages)?;
                self.inbox
                    .lock()
                    .enqueue(free)
                    .map_err(LocalHeapError::from)?;
                self.remote_pending.store(true, Ordering::Release);
                Ok(())
            }
        }
    }

    fn drain_if_pending(&self, pages: &mut PageMap) -> Result<(), LocalHeapError> {
        if self.remote_pending.load(Ordering::Acquire) {
            self.drain(pages)?;
        }

        Ok(())
    }

    pub(crate) fn drain(&self, pages: &mut PageMap) -> Result<(), LocalHeapError> {
        loop {
            let free = {
                let mut inbox = self.inbox.lock();
                let Some(free) = inbox.pop() else {
                    self.remote_pending.store(false, Ordering::Release);
                    return Ok(());
                };
                free
            };

            match free {
                RemoteFree::Run { run, ptr } => {
                    self.runs.lock().free(run, ptr, pages)?;
                }
                RemoteFree::Extent { extent, ptr } => {
                    self.extents.lock().free(extent, ptr, pages)?;
                }
            }

            self.release_allocation();
        }
    }

    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }

    pub(crate) fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    fn has_live_allocations(&self) -> bool {
        self.live.load(Ordering::Acquire) != 0
    }

    fn retain_allocation(&self) {
        let previous = self.live.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous < u32::MAX, "local heap live count overflow");
    }

    fn release_allocation(&self) {
        let previous = self.live.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "local heap live count underflow");
        if previous == 0 {
            self.live.store(0, Ordering::Release);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalHeapError {
    InvalidHeap,
    InvalidPointer,
    DoubleFree,
    InvalidMetadata,
}

impl From<RunHeapError> for LocalHeapError {
    fn from(error: RunHeapError) -> Self {
        match error {
            RunHeapError::InvalidPointer => Self::InvalidPointer,
            RunHeapError::DoubleFree => Self::DoubleFree,
            RunHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<ExtentHeapError> for LocalHeapError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent | ExtentHeapError::InvalidMetadata => {
                Self::InvalidMetadata
            }
            ExtentHeapError::InvalidPointer => Self::InvalidPointer,
        }
    }
}

impl From<RemoteError> for LocalHeapError {
    fn from(error: RemoteError) -> Self {
        match error {
            RemoteError::Full => Self::InvalidMetadata,
            RemoteError::Duplicate => Self::DoubleFree,
        }
    }
}

impl From<SlotStoreError> for LocalHeapError {
    fn from(_: SlotStoreError) -> Self {
        Self::InvalidMetadata
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteFree {
    Run {
        run: NonNull<Run>,
        ptr: NonNull<u8>,
    },
    Extent {
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteError {
    Full,
    Duplicate,
}

struct RemoteInbox {
    entries: [Option<RemoteFree>; REMOTE_INBOX_CAPACITY],
    head: usize,
    len: usize,
}

impl RemoteInbox {
    const fn new() -> Self {
        Self {
            entries: [None; REMOTE_INBOX_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn enqueue(&mut self, free: RemoteFree) -> Result<(), RemoteError> {
        if self.contains(free) {
            return Err(RemoteError::Duplicate);
        }
        if self.len == self.entries.len() {
            return Err(RemoteError::Full);
        }

        let index = (self.head + self.len) % self.entries.len();
        let Some(entry) = self.entries.get_mut(index) else {
            return Err(RemoteError::Full);
        };
        *entry = Some(free);
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<RemoteFree> {
        if self.len == 0 {
            return None;
        }

        let free = self.entries.get_mut(self.head)?.take()?;
        self.head = (self.head + 1) % self.entries.len();
        self.len -= 1;
        Some(free)
    }

    fn contains(&self, free: RemoteFree) -> bool {
        self.entries.iter().flatten().any(|entry| *entry == free)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LocalHeapHandle {
    id: HeapId,
    heap: NonNull<LocalHeap>,
}

impl LocalHeapHandle {
    fn new(id: HeapId, heap: NonNull<LocalHeap>) -> Self {
        Self { id, heap }
    }

    pub(crate) const fn id(self) -> HeapId {
        self.id
    }

    const fn heap_ptr(self) -> NonNull<LocalHeap> {
        self.heap
    }
}

pub(crate) struct ThreadHeaps {
    entries: [ThreadHeapEntry; THREAD_HEAP_CAPACITY],
}

struct ThreadHeapEntry {
    core: Cell<*mut AllocatorCore>,
    heap: Cell<Option<HeapId>>,
    local: Cell<*mut LocalHeap>,
}

impl Drop for ThreadHeaps {
    fn drop(&mut self) {
        for entry in &self.entries {
            let Some((core, heap)) = entry.take() else {
                continue;
            };

            // SAFETY: ThreadHeapEntry retains core while installed.
            let mut state = unsafe { core.as_ref() }.state().lock();
            let _ = state.abandon(heap);
            drop(state);

            AllocatorCore::release(core);
        }
    }
}

impl ThreadHeaps {
    const fn new() -> Self {
        Self {
            entries: [const { ThreadHeapEntry::new() }; THREAD_HEAP_CAPACITY],
        }
    }

    pub(crate) fn allocate_run(
        &self,
        core: NonNull<AllocatorCore>,
        class: SizeClassId,
    ) -> Option<Allocation> {
        let entry = self.entry(core)?;
        let local = NonNull::new(entry.local.get())?;

        // SAFETY: an installed entry retains core and points at an active local heap until take/drop.
        unsafe { local.as_ref() }.allocate_run(class)
    }

    pub(crate) fn heap_id(&self, core: NonNull<AllocatorCore>) -> Option<HeapId> {
        self.entry(core)?.heap.get()
    }

    pub(crate) fn get_or_acquire(
        &self,
        core: NonNull<AllocatorCore>,
        state: &mut AllocatorState,
    ) -> Option<HeapId> {
        if let Some(heap) = self.heap_id(core) {
            return Some(heap);
        }

        let entry = self.empty_entry()?;
        if !AllocatorCore::retain(core) {
            return None;
        }

        let Some(handle) = state.acquire_local_heap() else {
            AllocatorCore::release(core);
            return None;
        };
        let heap = handle.id();
        entry.install(core, handle);

        Some(heap)
    }

    fn entry(&self, core: NonNull<AllocatorCore>) -> Option<&ThreadHeapEntry> {
        self.entries
            .iter()
            .find(|entry| entry.core.get() == core.as_ptr())
    }

    fn empty_entry(&self) -> Option<&ThreadHeapEntry> {
        self.entries.iter().find(|entry| entry.core.get().is_null())
    }
}

impl ThreadHeapEntry {
    const fn new() -> Self {
        Self {
            core: Cell::new(core::ptr::null_mut()),
            heap: Cell::new(None),
            local: Cell::new(core::ptr::null_mut()),
        }
    }

    fn install(&self, core: NonNull<AllocatorCore>, handle: LocalHeapHandle) {
        self.local.set(handle.heap_ptr().as_ptr());
        self.heap.set(Some(handle.id()));
        self.core.set(core.as_ptr());
    }

    fn take(&self) -> Option<(NonNull<AllocatorCore>, HeapId)> {
        let core = NonNull::new(self.core.replace(core::ptr::null_mut()))?;
        let heap = self.heap.replace(None)?;
        self.local.set(core::ptr::null_mut());
        Some((core, heap))
    }
}

std::thread_local! {
    pub(crate) static THREAD_HEAPS: ThreadHeaps = const { ThreadHeaps::new() };
}
