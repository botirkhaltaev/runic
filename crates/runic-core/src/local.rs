use core::{
    cell::{Cell, UnsafeCell},
    ptr::NonNull,
};

use crate::{
    heap::SharedHeap,
    ownership::HeapId,
    run::{Run, RunBlock, RunError},
    size_class::{SizeClassId, SizeClasses},
};

const MAX_LOCAL_HEAPS: usize = 32;
const REMOTE_INBOX_CAPACITY: usize = 16;

pub(crate) struct HeapRegistry {
    slots: [HeapSlot; MAX_LOCAL_HEAPS],
}

// SAFETY: HeapRegistry is owned by SharedHeap, and SharedHeap is accessed through the allocator's
// global lock. Moving the registry between threads does not permit concurrent mutation.
unsafe impl Send for HeapRegistry {}

impl HeapRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { HeapSlot::new() }; MAX_LOCAL_HEAPS],
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<HeapId> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.active {
                continue;
            }

            slot.active = true;
            slot.owned = None;
            slot.inbox.clear();

            return HeapId::from_index(u32::try_from(index).ok()?);
        }

        None
    }

    pub(crate) fn attach_run(&mut self, id: HeapId, run: NonNull<Run>) -> bool {
        let Some(slot) = self.slot_mut(id) else {
            return false;
        };

        if !slot.active {
            return false;
        }

        // SAFETY: caller supplies a stable RunArena pointer assigned to this heap id.
        unsafe { run.as_ref() }.set_owner_next(slot.owned);
        slot.owned = Some(run);

        true
    }

    pub(crate) fn release(&mut self, id: HeapId) -> Option<NonNull<Run>> {
        let slot = self.slot_mut(id)?;

        if !slot.active {
            return None;
        }

        slot.active = false;
        slot.inbox.clear();

        slot.owned.take()
    }

    pub(crate) fn enqueue_remote(
        &mut self,
        id: HeapId,
        free: RemoteFree,
    ) -> Result<(), RemoteFree> {
        let Some(slot) = self.slot_mut(id) else {
            return Err(free);
        };

        if !slot.active {
            return Err(free);
        }

        slot.inbox.push(free)
    }

    pub(crate) fn pop_remote(&mut self, id: HeapId) -> Option<RemoteFree> {
        self.slot_mut(id)
            .filter(|slot| slot.active)
            .and_then(|slot| slot.inbox.pop())
    }

    pub(crate) fn has_remote(&self, id: HeapId) -> bool {
        self.slot(id)
            .filter(|slot| slot.active)
            .is_some_and(|slot| !slot.inbox.is_empty())
    }

    fn slot(&self, id: HeapId) -> Option<&HeapSlot> {
        self.slots.get(usize::try_from(id.index()).ok()?)
    }

    fn slot_mut(&mut self, id: HeapId) -> Option<&mut HeapSlot> {
        self.slots.get_mut(usize::try_from(id.index()).ok()?)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RemoteFree {
    run: NonNull<Run>,
    ptr: NonNull<u8>,
}

impl RemoteFree {
    pub(crate) const fn new(run: NonNull<Run>, ptr: NonNull<u8>) -> Self {
        Self { run, ptr }
    }

    pub(crate) const fn run(self) -> NonNull<Run> {
        self.run
    }

    pub(crate) const fn ptr(self) -> NonNull<u8> {
        self.ptr
    }
}

#[derive(Clone, Copy)]
struct HeapSlot {
    active: bool,
    owned: Option<NonNull<Run>>,
    inbox: RemoteInbox,
}

impl HeapSlot {
    const fn new() -> Self {
        Self {
            active: false,
            owned: None,
            inbox: RemoteInbox::new(),
        }
    }
}

#[derive(Clone, Copy)]
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

    fn push(&mut self, free: RemoteFree) -> Result<(), RemoteFree> {
        if self.len == self.entries.len() {
            return Err(free);
        }

        let index = (self.head + self.len) % self.entries.len();
        let Some(entry) = self.entries.get_mut(index) else {
            return Err(free);
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

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        while self.pop().is_some() {}
        self.head = 0;
    }
}

pub(crate) struct LocalHeap {
    id: HeapId,
    owned: Option<NonNull<Run>>,
    available: [Option<NonNull<Run>>; SizeClasses::COUNT],
}

impl LocalHeap {
    const fn new(id: HeapId) -> Self {
        Self {
            id,
            owned: None,
            available: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) const fn id(&self) -> HeapId {
        self.id
    }

    pub(crate) fn allocate(&mut self, class: SizeClassId) -> Option<NonNull<u8>> {
        let class_index = class.index();

        loop {
            if self.available.get(class_index)?.is_none() {
                self.cache_available_owned_run(class)?;
            }

            let run_ptr = *self.available.get(class_index)?.as_ref()?;
            let (ptr, next) = {
                // SAFETY: local available-list pointers are stable RunArena entries assigned to this LocalHeap.
                let run = unsafe { run_ptr.as_ref() };
                let ptr = run.allocate().map(RunBlock::ptr);

                if ptr.is_some() && run.has_available_blocks() {
                    return ptr;
                }

                (ptr, run.take_available_next())
            };

            *self.available.get_mut(class_index)? = next;

            if let Some(ptr) = ptr {
                return Some(ptr);
            }
        }
    }

    fn cache_available_owned_run(&mut self, class: SizeClassId) -> Option<()> {
        let mut current = self.owned;

        while let Some(run) = current {
            // SAFETY: owner-list pointers are stable RunArena entries.
            let run_ref = unsafe { run.as_ref() };
            current = run_ref.owner_next();

            if run_ref.class() == class && run_ref.has_available_blocks() {
                self.push_available(class, run);
                return Some(());
            }
        }

        None
    }

    pub(crate) fn push_available(&mut self, class: SizeClassId, run: NonNull<Run>) -> bool {
        let Some(head) = self.available.get_mut(class.index()) else {
            return false;
        };

        // SAFETY: caller supplies a stable RunArena pointer assigned to this LocalHeap.
        unsafe { run.as_ref() }.set_available_next(*head);
        *head = Some(run);

        true
    }

    pub(crate) fn attach_registered_run(&mut self, run: NonNull<Run>) {
        self.owned = Some(run);
    }

    pub(crate) fn free_local(
        &mut self,
        class: SizeClassId,
        ptr: NonNull<u8>,
    ) -> Result<bool, RunError> {
        let Some(run) = self.find_owned_run(class, ptr) else {
            return Ok(false);
        };

        // SAFETY: find_owned_run returns a stable RunArena pointer from this LocalHeap's owner list.
        let run_ref = unsafe { run.as_ref() };
        let was_full = run_ref.is_full();
        run_ref.free(ptr)?;

        if was_full {
            self.push_available(class, run);
        }

        Ok(true)
    }

    fn find_owned_run(&self, class: SizeClassId, ptr: NonNull<u8>) -> Option<NonNull<Run>> {
        let mut current = self.owned;

        while let Some(run) = current {
            // SAFETY: owner-list pointers are stable RunArena entries.
            let run_ref = unsafe { run.as_ref() };
            if run_ref.class() == class && run_ref.block_at(ptr).is_some() {
                return Some(run);
            }

            current = run_ref.owner_next();
        }

        None
    }
}

impl Drop for LocalSlot {
    fn drop(&mut self) {
        let key = self.key.get();
        if key == CellKey::NONE {
            return;
        }

        // SAFETY: this slot is being destroyed on its owning thread, so no nested mutable access exists.
        let Some(heap) = (unsafe { &mut *self.heap.get() }).take() else {
            return;
        };

        // SAFETY: allocator Drop clears same-thread local state before the allocator storage is
        // invalidated. Cross-thread local state can only exist while safe Rust keeps the allocator
        // alive, or for the process-lifetime global allocator.
        let shared = unsafe { &*key.0.cast::<spin::Mutex<SharedHeap>>() };
        let mut shared = shared.lock();
        shared.retire_local_heap(heap.id());
    }
}

struct LocalSlot {
    key: Cell<CellKey>,
    heap: UnsafeCell<Option<LocalHeap>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct CellKey(*const ());

impl CellKey {
    const NONE: Self = Self(core::ptr::null());
}

// SAFETY: each LocalSlot is thread-local. Interior mutation is confined to the owning thread.
unsafe impl Sync for LocalSlot {}

impl LocalSlot {
    const fn new() -> Self {
        Self {
            key: Cell::new(CellKey::NONE),
            heap: UnsafeCell::new(None),
        }
    }

    fn with<R>(&self, key: *const (), f: impl FnOnce(&mut LocalHeap) -> R) -> Option<R> {
        if self.key.get() != CellKey(key) {
            return None;
        }

        // SAFETY: this slot is thread-local and no nested mutable access is created by this method.
        unsafe { &mut *self.heap.get() }.as_mut().map(f)
    }

    fn accepts(&self, key: *const ()) -> bool {
        matches!(self.key.get(), CellKey::NONE) || self.key.get() == CellKey(key)
    }

    fn with_or_init<R>(
        &self,
        key: *const (),
        id: HeapId,
        f: impl FnOnce(&mut LocalHeap) -> R,
    ) -> Option<R> {
        // SAFETY: this slot is thread-local and no nested mutable access is created by this method.
        let heap = unsafe { &mut *self.heap.get() };

        if self.key.get() == CellKey::NONE {
            self.key.set(CellKey(key));
            *heap = Some(LocalHeap::new(id));
        }

        if self.key.get() != CellKey(key) {
            return None;
        }

        heap.as_mut().map(f)
    }

    fn take(&self, key: *const ()) -> Option<LocalHeap> {
        if self.key.get() != CellKey(key) {
            return None;
        }

        self.key.set(CellKey::NONE);
        // SAFETY: this slot is thread-local and no nested mutable access is created by this method.
        unsafe { &mut *self.heap.get() }.take()
    }
}

std::thread_local! {
    static LOCAL_HEAP: LocalSlot = const { LocalSlot::new() };
}

pub(crate) fn with_local<R>(key: *const (), f: impl FnOnce(&mut LocalHeap) -> R) -> Option<R> {
    LOCAL_HEAP.with(|slot| slot.with(key, f))
}

pub(crate) fn accepts_local(key: *const ()) -> bool {
    LOCAL_HEAP.with(|slot| slot.accepts(key))
}

pub(crate) fn with_local_or_init<R>(
    key: *const (),
    id: HeapId,
    f: impl FnOnce(&mut LocalHeap) -> R,
) -> Option<R> {
    LOCAL_HEAP.with(|slot| slot.with_or_init(key, id, f))
}

pub(crate) fn take_local(key: *const ()) -> Option<LocalHeap> {
    LOCAL_HEAP.with(|slot| slot.take(key))
}
