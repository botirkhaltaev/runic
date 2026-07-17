use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::{
    allocation::{Allocation, ZeroStatus},
    config::AllocatorConfig,
    extent::{Extent, ExtentHeap, ExtentHeapError},
    heap::SharedHeap,
    layout::LayoutSpec,
    local::{self, LocalHeapError, LocalHeapTable, RemoteFree},
    memory::{OsMemory, PageMap, PageOwner},
    ownership::{HeapId, HeapOwner},
    run::{Run, RunHeap, RunHeapError},
    size_class::SizeClasses,
};

pub struct Allocator {
    config: AllocatorConfig,
    core: AtomicPtr<AllocatorCore>,
}

pub(crate) struct AllocatorCore {
    refs: AtomicU32,
    mapping_len: usize,
    state: Mutex<AllocatorState>,
}

pub(crate) struct AllocatorState {
    pages: PageMap,
    shared: SharedHeap,
    locals: LocalHeapTable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocatorError {
    UnknownPointer,
    MissingExtent,
    InvalidRunPointer,
    InvalidExtentPointer,
    DoubleFree,
    InvalidMetadata,
}

impl Allocator {
    #[must_use]
    pub const fn new() -> Self {
        Self::with_config(AllocatorConfig::new())
    }

    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            config,
            core: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Allocates memory for `layout` using this allocator's state.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw, uninitialized memory. The caller must use it
    /// only according to `layout`, avoid out-of-bounds access, and eventually
    /// pass the same pointer and a compatible layout back to this allocator.
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let Some(core) = self.core() else {
            return null_mut();
        };

        if let Some(class) = SizeClasses::id_for(spec)
            && let Some(allocation) =
                local::THREAD_HEAPS.with(|heaps| heaps.allocate_run(core, class))
        {
            return allocation.ptr().as_ptr();
        }

        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let mut state = unsafe { core.as_ref() }.state().lock();
        let heap = local::THREAD_HEAPS.with(|heaps| heaps.get_or_acquire(core, &mut state));

        state
            .allocate(heap, spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr())
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `layout`. Passing an unknown pointer, an interior pointer, or an
    /// incompatible layout violates the allocator contract and may abort.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        let Some(core) = self.loaded_core() else {
            Self::abort();
        };
        let current_heap = local::THREAD_HEAPS.with(|heaps| heaps.heap_id(core));
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let mut state = unsafe { core.as_ref() }.state().lock();
        if state.dealloc(ptr, layout, current_heap).is_err() {
            Self::abort();
        }
    }

    /// Changes the size of an allocation using allocate-copy-free semantics.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `old`. If a non-null pointer is supplied, no other live reference may
    /// be used to access the old allocation after successful reallocation.
    pub unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let core = if ptr.is_null() {
            let Some(core) = self.core() else {
                return null_mut();
            };
            core
        } else {
            let Some(core) = self.loaded_core() else {
                Self::abort();
            };
            core
        };
        let current_heap = local::THREAD_HEAPS.with(|heaps| heaps.heap_id(core));
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let mut state = unsafe { core.as_ref() }.state().lock();
        state
            .realloc(ptr, old, new_size, current_heap)
            .unwrap_or_else(|_| Self::abort())
    }

    /// Allocates zero-initialized memory for `layout`.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw memory. The caller must use it only according
    /// to `layout` and eventually pass it back to this allocator with a
    /// compatible layout.
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let Some(core) = self.core() else {
            return null_mut();
        };
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let mut state = unsafe { core.as_ref() }.state().lock();
        let heap = local::THREAD_HEAPS.with(|heaps| heaps.get_or_acquire(core, &mut state));

        state.alloc_zeroed(layout, heap)
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }

    fn core(&self) -> Option<NonNull<AllocatorCore>> {
        if let Some(core) = self.loaded_core() {
            return Some(core);
        }

        let core = AllocatorCore::new(self.config)?;
        match self.core.compare_exchange(
            core::ptr::null_mut(),
            core.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(core),
            Err(existing) => {
                AllocatorCore::release(core);
                NonNull::new(existing)
            }
        }
    }

    fn loaded_core(&self) -> Option<NonNull<AllocatorCore>> {
        NonNull::new(self.core.load(Ordering::Acquire))
    }
}

impl AllocatorCore {
    fn new(config: AllocatorConfig) -> Option<NonNull<Self>> {
        let mapping = OsMemory::map(core::mem::size_of::<Self>())?;
        let mapping_len = mapping.range().len();
        let core = mapping.base().cast::<Self>();
        core::mem::forget(mapping);

        // SAFETY: core points to uniquely owned mmap storage aligned at least to a page boundary.
        unsafe {
            core.as_ptr().write(Self {
                refs: AtomicU32::new(1),
                mapping_len,
                state: Mutex::new(AllocatorState::with_config(config)),
            });
        }

        Some(core)
    }

    pub(crate) fn retain(core: NonNull<Self>) -> bool {
        // SAFETY: callers obtain core from an Allocator or an existing retained TLS entry.
        let refs = unsafe { &core.as_ref().refs };
        let mut current = refs.load(Ordering::Acquire);

        loop {
            if current == 0 {
                return false;
            }

            let Some(next) = current.checked_add(1) else {
                Self::abort();
            };

            match refs.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn release(core: NonNull<Self>) {
        // SAFETY: callers release one previously retained reference to this live core.
        let refs = unsafe { &core.as_ref().refs };
        let mut current = refs.load(Ordering::Acquire);

        loop {
            if current == 0 {
                Self::abort();
            }

            let next = current - 1;
            match refs.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if next == 0 {
                        // SAFETY: this was the final reference, so no thread can access core after this point.
                        unsafe { Self::destroy(core) };
                    }
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn state(&self) -> &Mutex<AllocatorState> {
        &self.state
    }

    unsafe fn destroy(core: NonNull<Self>) {
        // SAFETY: caller guarantees this is the final reference to core.
        let mapping_len = unsafe { core.as_ref().mapping_len };
        // SAFETY: core points to an initialized AllocatorCore in uniquely owned mmap storage.
        unsafe { core.as_ptr().drop_in_place() };
        // SAFETY: storage was allocated by OsMemory::map and is no longer occupied by AllocatorCore.
        unsafe { OsMemory::unmap(core.cast::<u8>(), mapping_len) };
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }
}

impl AllocatorState {
    pub(crate) const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            pages: PageMap::new(),
            shared: SharedHeap::with_config(config),
            locals: LocalHeapTable::new(config),
        }
    }

    pub(crate) fn abandon(&mut self, heap: HeapId) -> Result<(), AllocatorError> {
        self.locals.abandon(heap, &mut self.pages)?;
        self.locals.reclaim(heap)?;
        Ok(())
    }

    pub(crate) fn acquire_local_heap(&mut self) -> Option<local::LocalHeapHandle> {
        self.locals.acquire()
    }

    fn allocate(&mut self, current_heap: Option<HeapId>, spec: LayoutSpec) -> Option<Allocation> {
        if let Some(heap_id) = current_heap
            && let Some(heap) = self.locals.get_mut(heap_id)
            && !heap.is_abandoned()
        {
            let allocation = match SizeClasses::id_for(spec) {
                Some(class) => heap
                    .allocate_run(class)
                    .or_else(|| heap.allocate_run_slow(class, &mut self.pages)),
                None => heap.allocate_extent(spec, &mut self.pages),
            };

            if allocation.is_some() {
                return allocation;
            }
        }

        self.shared.allocate(spec, &mut self.pages)
    }

    fn dealloc(
        &mut self,
        raw_ptr: *mut u8,
        _layout: Layout,
        current_heap: Option<HeapId>,
    ) -> Result<(), AllocatorError> {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return Ok(());
        };

        let Some(entry) = self.pages.get(ptr) else {
            return Err(AllocatorError::UnknownPointer);
        };

        match entry {
            PageOwner::Run(run) => self.dealloc_run(run, ptr, current_heap)?,
            PageOwner::Extent(extent) => self.dealloc_extent(extent, ptr, current_heap)?,
        }

        Ok(())
    }

    fn dealloc_run(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        match unsafe { run.as_ref() }.owner() {
            HeapOwner::Shared => self.shared.free_run(run, ptr, &mut self.pages)?,
            HeapOwner::Local(heap_id) if Some(heap_id) == current_heap => {
                let heap = self
                    .locals
                    .get_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                heap.free_run(run, ptr, &mut self.pages)?;
                self.locals.reclaim(heap_id)?;
            }
            HeapOwner::Local(heap_id) => {
                let heap = self
                    .locals
                    .get_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                if heap.is_abandoned() {
                    heap.free_run(run, ptr, &mut self.pages)?;
                    self.locals.reclaim(heap_id)?;
                } else {
                    // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
                    unsafe { run.as_ref() }
                        .validate_free(ptr)
                        .map_err(RunHeapError::from)?;
                    heap.enqueue(RemoteFree::Run { run, ptr }, &mut self.pages)?;
                }
            }
        }

        Ok(())
    }

    fn dealloc_extent(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
        match unsafe { extent.as_ref() }.owner() {
            HeapOwner::Shared => self.shared.free_extent(extent, ptr, &mut self.pages)?,
            HeapOwner::Local(heap_id) if Some(heap_id) == current_heap => {
                let heap = self
                    .locals
                    .get_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                heap.free_extent(extent, ptr, &mut self.pages)?;
                self.locals.reclaim(heap_id)?;
            }
            HeapOwner::Local(heap_id) => {
                let heap = self
                    .locals
                    .get_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                if heap.is_abandoned() {
                    heap.free_extent(extent, ptr, &mut self.pages)?;
                    self.locals.reclaim(heap_id)?;
                } else {
                    // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
                    unsafe { extent.as_ref() }
                        .validate_free(ptr)
                        .map_err(|_| AllocatorError::InvalidExtentPointer)?;
                    heap.enqueue(RemoteFree::Extent { extent, ptr }, &mut self.pages)?;
                }
            }
        }

        Ok(())
    }

    fn realloc(
        &mut self,
        ptr: *mut u8,
        old: Layout,
        new_size: usize,
        current_heap: Option<HeapId>,
    ) -> Result<*mut u8, AllocatorError> {
        if ptr.is_null() {
            let Some(spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
                return Ok(null_mut());
            };

            return Ok(self
                .allocate(current_heap, spec)
                .map_or(null_mut(), |allocation| allocation.ptr().as_ptr()));
        }

        if new_size == 0 {
            self.dealloc(ptr, old, current_heap)?;
            return Ok(null_mut());
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return Ok(null_mut());
        };

        let Some(entry) = self.pages.get(old_ptr) else {
            return Err(AllocatorError::UnknownPointer);
        };

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return Ok(null_mut());
        };
        let new_spec = LayoutSpec::from_layout(new_layout);

        match entry {
            PageOwner::Run(run) => {
                if RunHeap::resize_in_place(run, old_ptr, new_spec).map_err(AllocatorError::from)? {
                    return Ok(ptr);
                }
            }
            PageOwner::Extent(extent) => {
                if ExtentHeap::resize_in_place(extent, old_ptr, new_spec)
                    .map_err(AllocatorError::from)?
                {
                    return Ok(ptr);
                }
            }
        }

        let new_ptr = self
            .allocate(current_heap, new_spec)
            .map_or(null_mut(), |allocation| allocation.ptr().as_ptr());

        if new_ptr.is_null() {
            return Ok(null_mut());
        }

        // SAFETY: new_ptr is a fresh allocation of at least new_layout.size() bytes; ptr is valid for old.size().
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_layout.size())) };

        if let Err(error) = self.dealloc(ptr, old, current_heap) {
            let _ = self.dealloc(new_ptr, new_layout, current_heap);

            return Err(error);
        }

        Ok(new_ptr)
    }

    fn alloc_zeroed(&mut self, layout: Layout, current_heap: Option<HeapId>) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let Some(allocation) = self.allocate(current_heap, spec) else {
            return null_mut();
        };
        let ptr = allocation.ptr().as_ptr();

        if allocation.zero_status() == ZeroStatus::NeedsZeroing {
            // SAFETY: ptr is valid for layout.size() bytes because it was just allocated for layout.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }

        ptr
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        let core = self.core.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if let Some(core) = NonNull::new(core) {
            AllocatorCore::release(core);
        }
    }
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl From<RunHeapError> for AllocatorError {
    fn from(error: RunHeapError) -> Self {
        match error {
            RunHeapError::InvalidPointer => Self::InvalidRunPointer,
            RunHeapError::DoubleFree => Self::DoubleFree,
            RunHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<ExtentHeapError> for AllocatorError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent => Self::MissingExtent,
            ExtentHeapError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentHeapError::InvalidMetadata => Self::InvalidMetadata,
        }
    }
}

impl From<LocalHeapError> for AllocatorError {
    fn from(error: LocalHeapError) -> Self {
        match error {
            LocalHeapError::InvalidHeap | LocalHeapError::InvalidMetadata => Self::InvalidMetadata,
            LocalHeapError::InvalidPointer => Self::InvalidRunPointer,
            LocalHeapError::DoubleFree => Self::DoubleFree,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AllocatorState {
        AllocatorState::with_config(AllocatorConfig::new())
    }

    #[test]
    fn allocator_state_reports_small_double_free() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = state
            .allocate(None, LayoutSpec::from_layout(layout))
            .unwrap()
            .ptr();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_reports_small_realloc_after_free() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = state
            .allocate(None, LayoutSpec::from_layout(layout))
            .unwrap()
            .ptr();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(
            state.realloc(ptr.as_ptr(), layout, 128, None),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_reports_large_double_free_as_unknown_pointer() {
        let mut state = state();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = state
            .allocate(None, LayoutSpec::from_layout(layout))
            .unwrap()
            .ptr();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None),
            Err(AllocatorError::UnknownPointer)
        );
    }

    #[test]
    fn allocator_state_allocates_small_from_local_heap() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let handle = state.locals.acquire().unwrap();
        let allocation = state
            .allocate(Some(handle.id()), LayoutSpec::from_layout(layout))
            .unwrap();
        let ptr = allocation.ptr();
        let PageOwner::Run(run) = state.pages.get(ptr).unwrap() else {
            panic!("small local allocation should publish a run");
        };

        // SAFETY: PageMap stores only live run pointers.
        assert_eq!(
            unsafe { run.as_ref() }.owner(),
            HeapOwner::Local(handle.id())
        );
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id())),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_allocates_extent_from_local_heap() {
        let mut state = state();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let handle = state.locals.acquire().unwrap();
        let allocation = state
            .allocate(Some(handle.id()), LayoutSpec::from_layout(layout))
            .unwrap();
        let ptr = allocation.ptr();
        let PageOwner::Extent(extent) = state.pages.get(ptr).unwrap() else {
            panic!("large local allocation should publish an extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(
            unsafe { extent.as_ref() }.owner(),
            HeapOwner::Local(handle.id())
        );
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id())),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_rejects_duplicate_remote_free_in_inbox() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let handle = state.locals.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), LayoutSpec::from_layout(layout))
            .unwrap()
            .ptr();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_tracks_fast_local_run_allocations_before_reclaim() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let class = SizeClasses::id_for(spec).unwrap();
        let handle = state.locals.acquire().unwrap();
        let first = state.allocate(Some(handle.id()), spec).unwrap().ptr();
        let second = state
            .locals
            .get_mut(handle.id())
            .unwrap()
            .allocate_run(class)
            .unwrap()
            .ptr();

        assert_eq!(state.abandon(handle.id()), Ok(()));
        assert_eq!(state.dealloc(first.as_ptr(), layout, None), Ok(()));
        assert_eq!(state.dealloc(second.as_ptr(), layout, None), Ok(()));
    }

    #[test]
    fn allocator_state_reclaim_removes_abandoned_local_run_page_entry() {
        let mut state = state();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let handle = state.locals.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), LayoutSpec::from_layout(layout))
            .unwrap()
            .ptr();

        assert_eq!(state.abandon(handle.id()), Ok(()));
        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None), Ok(()));
        assert_eq!(state.pages.get(ptr), None);
    }

    #[test]
    fn allocator_state_zeroed_allocation_uses_current_local_heap() {
        let mut state = state();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let handle = state.locals.acquire().unwrap();
        let ptr = NonNull::new(state.alloc_zeroed(layout, Some(handle.id()))).unwrap();
        let PageOwner::Extent(extent) = state.pages.get(ptr).unwrap() else {
            panic!("large local allocation should publish an extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(
            unsafe { extent.as_ref() }.owner(),
            HeapOwner::Local(handle.id())
        );
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id())),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_realloc_growth_uses_current_local_heap() {
        let mut state = state();
        let small = Layout::from_size_align(64, 8).unwrap();
        let large = Layout::from_size_align(128 * 1024, 8).unwrap();
        let handle = state.locals.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), LayoutSpec::from_layout(small))
            .unwrap()
            .ptr();

        // SAFETY: ptr was allocated for small.size() bytes above.
        unsafe { write_bytes(ptr.as_ptr(), 0xab, small.size()) };
        let grown = state
            .realloc(ptr.as_ptr(), small, large.size(), Some(handle.id()))
            .unwrap();
        let grown = NonNull::new(grown).unwrap();
        let PageOwner::Extent(extent) = state.pages.get(grown).unwrap() else {
            panic!("grown local allocation should publish an extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(
            unsafe { extent.as_ref() }.owner(),
            HeapOwner::Local(handle.id())
        );
        assert_eq!(
            state.dealloc(grown.as_ptr(), large, Some(handle.id())),
            Ok(())
        );
    }
}
