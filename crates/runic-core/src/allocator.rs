use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::{
    config::AllocatorConfig,
    heap::{
        Extent, ExtentHeap, ExtentHeapError, Heap, HeapError, HeapHandle, HeapId, HeapTable, Run,
        RunHeap, RunHeapError, RunOwner, THREAD_HEAP,
    },
    layout::LayoutSpec,
    memory::{OsMemory, PageMap, PageOwner},
    size_class::{SizeClassId, SizeClasses},
};

pub struct Allocator {
    config: AllocatorConfig,
    core: AtomicPtr<AllocatorCore>,
}

pub(crate) struct AllocatorCore {
    refs: AtomicU32,
    mapping_len: usize,
    pages: PageMap,
    state: Mutex<AllocatorState>,
}

pub(crate) struct AllocatorState {
    root: Heap,
    heaps: HeapTable,
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

        let class = SizeClasses::id_for(spec);
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let core_ref = unsafe { core.as_ref() };
        if let Some(class) = class
            && let Some(ptr) =
                THREAD_HEAP.with(|heap| heap.allocate_run(core, class, core_ref.pages()))
        {
            return ptr.as_ptr();
        }

        if class.is_some() {
            THREAD_HEAP.with(|heap| heap.release_if_different(core));
        }
        let mut state = core_ref.state().lock();
        let heap =
            class.and_then(|_| THREAD_HEAP.with(|heap| heap.get_or_acquire(core, &mut state)));
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let pages = core_ref.pages();

        state
            .allocate(heap, class, spec, pages)
            .map_or(null_mut(), NonNull::as_ptr)
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `layout`. Passing an unknown pointer, an interior pointer, or an
    /// incompatible layout violates the allocator contract and may abort.
    pub unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }

        let Some(core) = self.loaded_core() else {
            Self::abort();
        };
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let core_ref = unsafe { core.as_ref() };

        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };

        let Some(entry) = core_ref.pages().get(ptr) else {
            Self::abort();
        };

        if let PageOwner::Run(run) = entry {
            // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
            if let RunOwner::Thread(heap) = unsafe { run.as_ref() }.owner()
                && let Some(result) =
                    THREAD_HEAP.with(|thread_heap| thread_heap.free_run(core, heap, run, ptr))
            {
                if result.is_err() {
                    Self::abort();
                }
                return;
            }
        }

        if Self::dealloc_slow(core, core_ref, entry, ptr).is_err() {
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
        let current_heap = THREAD_HEAP.with(|heap| heap.heap_id(core));
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let core_ref = unsafe { core.as_ref() };
        let mut state = core_ref.state().lock();
        state
            .realloc(ptr, old, new_size, current_heap, core_ref.pages())
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
        let spec = LayoutSpec::from_layout(layout);
        let Some(core) = self.core() else {
            return null_mut();
        };

        let class = SizeClasses::id_for(spec);
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let core_ref = unsafe { core.as_ref() };
        if let Some(class) = class
            && let Some(ptr) =
                THREAD_HEAP.with(|heap| heap.allocate_run(core, class, core_ref.pages()))
        {
            // SAFETY: ptr was just allocated for layout and is valid for layout.size() bytes.
            unsafe { write_bytes(ptr.as_ptr(), 0, layout.size()) };
            return ptr.as_ptr();
        }

        if class.is_some() {
            THREAD_HEAP.with(|heap| heap.release_if_different(core));
        }
        let mut state = core_ref.state().lock();
        let heap =
            class.and_then(|_| THREAD_HEAP.with(|heap| heap.get_or_acquire(core, &mut state)));

        state.alloc_zeroed(spec, layout.size(), class, heap, core_ref.pages())
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

        self.initialize_core()
    }

    #[cold]
    #[inline(never)]
    fn initialize_core(&self) -> Option<NonNull<AllocatorCore>> {
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

    #[cold]
    #[inline(never)]
    fn dealloc_slow(
        core: NonNull<AllocatorCore>,
        core_ref: &AllocatorCore,
        entry: PageOwner,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let current_heap = THREAD_HEAP.with(|heap| heap.heap_id(core));
        let mut state = core_ref.state().lock();
        state.dealloc_owner(entry, ptr, current_heap, core_ref.pages())
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
                pages: PageMap::new(),
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

    pub(crate) const fn pages(&self) -> &PageMap {
        &self.pages
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
            root: Heap::root(config),
            heaps: HeapTable::new(config),
        }
    }

    pub(crate) fn abandon(&mut self, heap: HeapId, pages: &PageMap) -> Result<(), AllocatorError> {
        self.heaps.abandon(heap, pages)?;
        self.heaps.reclaim(heap, pages)?;
        Ok(())
    }

    pub(crate) fn acquire_heap(&mut self) -> Option<HeapHandle> {
        self.heaps.acquire()
    }

    fn allocate(
        &mut self,
        current_heap: Option<HeapId>,
        class: Option<SizeClassId>,
        spec: LayoutSpec,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        if let Some(heap_id) = current_heap
            && let Some(class) = class
            && let Some(heap) = self.heaps.active_heap(heap_id)
        {
            let allocation = heap.allocate_run(class, pages);

            if allocation.is_some() {
                return allocation;
            }
        }

        match class {
            Some(class) => self.root.allocate_run(class, pages),
            None => self.root.allocate_extent(spec, pages),
        }
    }

    fn dealloc(
        &mut self,
        raw_ptr: *mut u8,
        _layout: Layout,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return Ok(());
        };

        let Some(entry) = pages.get(ptr) else {
            return Err(AllocatorError::UnknownPointer);
        };

        self.dealloc_owner(entry, ptr, current_heap, pages)
    }

    fn dealloc_owner(
        &mut self,
        entry: PageOwner,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        match entry {
            PageOwner::Run(run) => self.dealloc_run(run, ptr, current_heap, pages)?,
            PageOwner::Extent(extent) => {
                self.dealloc_extent(extent, ptr, current_heap, pages)?;
            }
        }

        Ok(())
    }

    fn dealloc_run(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live RunArena.
        let owner = unsafe { run.as_ref() }.owner();
        match owner {
            RunOwner::Central => self.root.free_run(run, ptr)?,
            RunOwner::Thread(heap_id) => {
                let heap = self.heaps.heap_ref(heap_id)?;
                if Some(heap_id) == current_heap || heap.is_abandoned() {
                    heap.free_run(run, ptr)?;
                    self.heaps.reclaim(heap_id, pages)?;
                } else {
                    heap.free_remote_run(run, ptr)?;
                }
            }
        }

        Ok(())
    }

    fn dealloc_extent(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        _current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live ExtentArena.
        let owner = unsafe { extent.as_ref() }.owner();
        if owner.is_root() {
            self.root.free_extent(extent, ptr, pages)?;
        } else {
            return Err(AllocatorError::InvalidMetadata);
        }

        Ok(())
    }

    fn realloc(
        &mut self,
        ptr: *mut u8,
        old: Layout,
        new_size: usize,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<*mut u8, AllocatorError> {
        if ptr.is_null() {
            let Some(spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
                return Ok(null_mut());
            };

            return Ok(self
                .allocate(current_heap, SizeClasses::id_for(spec), spec, pages)
                .map_or(null_mut(), NonNull::as_ptr));
        }

        if new_size == 0 {
            self.dealloc(ptr, old, current_heap, pages)?;
            return Ok(null_mut());
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return Ok(null_mut());
        };

        let Some(entry) = pages.get(old_ptr) else {
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
            .allocate(current_heap, SizeClasses::id_for(new_spec), new_spec, pages)
            .map_or(null_mut(), NonNull::as_ptr);

        if new_ptr.is_null() {
            return Ok(null_mut());
        }

        // SAFETY: new_ptr is a fresh allocation of at least new_layout.size() bytes; ptr is valid for old.size().
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_layout.size())) };

        if let Err(error) = self.dealloc(ptr, old, current_heap, pages) {
            let _ = self.dealloc(new_ptr, new_layout, current_heap, pages);

            return Err(error);
        }

        Ok(new_ptr)
    }

    fn alloc_zeroed(
        &mut self,
        spec: LayoutSpec,
        requested_size: usize,
        class: Option<SizeClassId>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> *mut u8 {
        match class {
            Some(class) => {
                if let Some(heap_id) = current_heap
                    && let Some(heap) = self.heaps.active_heap(heap_id)
                    && let Some(ptr) = heap.allocate_run(class, pages)
                {
                    // SAFETY: ptr was just allocated for this layout and is valid for requested_size bytes.
                    unsafe { write_bytes(ptr.as_ptr(), 0, requested_size) };
                    return ptr.as_ptr();
                }

                self.root
                    .allocate_run(class, pages)
                    .map_or(null_mut(), |ptr| {
                        // SAFETY: ptr was just allocated for this layout and is valid for requested_size bytes.
                        unsafe { write_bytes(ptr.as_ptr(), 0, requested_size) };
                        ptr.as_ptr()
                    })
            }
            None => self
                .root
                .allocate_zeroed_extent(spec, requested_size, pages)
                .map_or(null_mut(), NonNull::as_ptr),
        }
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

impl From<HeapError> for AllocatorError {
    fn from(error: HeapError) -> Self {
        match error {
            HeapError::InvalidHeap | HeapError::InvalidMetadata => Self::InvalidMetadata,
            HeapError::InvalidPointer => Self::InvalidRunPointer,
            HeapError::DoubleFree => Self::DoubleFree,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::write_bytes;

    use super::*;

    #[test]
    fn allocator_state_reports_small_double_free() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let ptr = state
            .allocate(None, SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None, &pages), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None, &pages),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_reports_small_realloc_after_free() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let ptr = state
            .allocate(None, SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None, &pages), Ok(()));
        assert_eq!(
            state.realloc(ptr.as_ptr(), layout, 128, None, &pages),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_reports_large_double_free_as_unknown_pointer() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let ptr = state
            .allocate(None, SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None, &pages), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None, &pages),
            Err(AllocatorError::UnknownPointer)
        );
    }

    #[test]
    fn allocator_state_allocates_small_from_current_heap() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();
        let PageOwner::Run(run) = pages.get(ptr).unwrap() else {
            panic!("small current-heap allocation should publish a run");
        };

        // SAFETY: PageMap stores only live run pointers.
        assert_eq!(
            unsafe { run.as_ref() }.owner(),
            RunOwner::Thread(handle.id())
        );
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id()), &pages),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_allocates_extent_from_central_heap() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();
        let PageOwner::Extent(extent) = pages.get(ptr).unwrap() else {
            panic!("large allocation should publish a central extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.owner(), HeapId::ROOT);
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id()), &pages),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_rejects_duplicate_remote_free() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None, &pages), Ok(()));
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, None, &pages),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_state_tracks_fast_current_heap_run_allocations_before_reclaim() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let class = SizeClasses::id_for(spec).unwrap();
        let handle = state.heaps.acquire().unwrap();
        let first = state
            .allocate(Some(handle.id()), Some(class), spec, &pages)
            .unwrap();
        let second = state
            .heaps
            .active_heap(handle.id())
            .unwrap()
            .allocate_run(class, &pages)
            .unwrap();

        assert_eq!(state.abandon(handle.id(), &pages), Ok(()));
        assert_eq!(state.dealloc(first.as_ptr(), layout, None, &pages), Ok(()));
        assert_eq!(state.dealloc(second.as_ptr(), layout, None, &pages), Ok(()));
    }

    #[test]
    fn allocator_state_reuses_abandoned_heap_after_remote_free() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let heap = handle.id();
        let ptr = state
            .allocate(Some(heap), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(state.abandon(heap, &pages), Ok(()));
        assert_eq!(state.dealloc(ptr.as_ptr(), layout, None, &pages), Ok(()));
        assert!(pages.get(ptr).is_some());
        assert_eq!(state.heaps.acquire().unwrap().id(), heap);
    }

    #[test]
    fn allocator_state_abandon_retains_empty_heap_run_page_entry_for_reuse() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let heap = handle.id();
        let ptr = state
            .allocate(Some(heap), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(heap), &pages),
            Ok(())
        );
        assert!(pages.get(ptr).is_some());
        assert_eq!(state.abandon(heap, &pages), Ok(()));
        assert!(pages.get(ptr).is_some());

        let reused = state.heaps.acquire().unwrap();
        assert_eq!(reused.id(), heap);
        let reused_ptr = state
            .allocate(Some(reused.id()), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();
        assert_eq!(reused_ptr, ptr);
        assert_eq!(
            state.dealloc(reused_ptr.as_ptr(), layout, Some(reused.id()), &pages),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_zeroed_large_allocation_uses_central_heap() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let spec = LayoutSpec::from_layout(layout);
        let handle = state.heaps.acquire().unwrap();
        let ptr = NonNull::new(state.alloc_zeroed(
            spec,
            layout.size(),
            SizeClasses::id_for(spec),
            Some(handle.id()),
            &pages,
        ))
        .unwrap();
        let PageOwner::Extent(extent) = pages.get(ptr).unwrap() else {
            panic!("large zeroed allocation should publish a central extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.owner(), HeapId::ROOT);
        assert_eq!(
            state.dealloc(ptr.as_ptr(), layout, Some(handle.id()), &pages),
            Ok(())
        );
    }

    #[test]
    fn allocator_state_realloc_growth_uses_central_extent() {
        let mut state = AllocatorState::with_config(AllocatorConfig::new());
        let pages = PageMap::new();
        let small = Layout::from_size_align(64, 8).unwrap();
        let large = Layout::from_size_align(128 * 1024, 8).unwrap();
        let spec = LayoutSpec::from_layout(small);
        let handle = state.heaps.acquire().unwrap();
        let ptr = state
            .allocate(Some(handle.id()), SizeClasses::id_for(spec), spec, &pages)
            .unwrap();

        // SAFETY: ptr was allocated for small.size() bytes above.
        unsafe { write_bytes(ptr.as_ptr(), 0xab, small.size()) };
        let grown = state
            .realloc(ptr.as_ptr(), small, large.size(), Some(handle.id()), &pages)
            .unwrap();
        let grown = NonNull::new(grown).unwrap();
        let PageOwner::Extent(extent) = pages.get(grown).unwrap() else {
            panic!("grown allocation should publish a central extent");
        };

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.owner(), HeapId::ROOT);
        assert_eq!(
            state.dealloc(grown.as_ptr(), large, Some(handle.id()), &pages),
            Ok(())
        );
    }
}
