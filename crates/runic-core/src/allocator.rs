use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use spin::Mutex;

use crate::{
    config::AllocatorConfig,
    heap::extent::ExtentError,
    heap::{
        Extent, ExtentHeap, ExtentHeapError, ExtentInit, Heap, HeapError, HeapId, HeapMode,
        HeapTable, Run, RunError, RunHeap, RunHeapError, THREAD_HEAP,
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
    pub(crate) heaps: HeapTable,
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
        let heap = THREAD_HEAP.with(|heap| heap.get_or_acquire(core, &mut state));
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
            // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
            let heap_id = unsafe { run.as_ref() }.heap_id();
            if let Some(result) =
                THREAD_HEAP.with(|thread_heap| thread_heap.free_run(core, heap_id, run, ptr))
            {
                if result.is_err() {
                    Self::abort();
                }
                return;
            }
        }

        if Self::dealloc_slow(core, core_ref, ptr).is_err() {
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
        if ptr.is_null() {
            let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
                return null_mut();
            };
            // SAFETY: the returned pointer is used only as a fresh allocation for new_layout.
            return unsafe { self.alloc(new_layout) };
        }

        if new_size == 0 {
            // SAFETY: ptr is non-null and the caller guarantees it was returned for old.
            unsafe { self.dealloc(ptr, old) };
            return null_mut();
        }

        let Some(core) = self.loaded_core() else {
            Self::abort();
        };
        // SAFETY: core is retained by this Allocator while loaded from self.core.
        let core_ref = unsafe { core.as_ref() };

        let Some(old_ptr) = NonNull::new(ptr) else {
            return null_mut();
        };
        let Some(entry) = core_ref.pages().get(old_ptr) else {
            Self::abort();
        };

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return null_mut();
        };
        let new_spec = LayoutSpec::from_layout(new_layout);

        let resized = match entry {
            PageOwner::Run(run) => {
                RunHeap::resize_in_place(run, old_ptr, new_spec).map_err(AllocatorError::from)
            }
            PageOwner::Extent(extent) => {
                ExtentHeap::resize_in_place(extent, old_ptr, new_spec).map_err(AllocatorError::from)
            }
        };
        match resized {
            Ok(true) => return ptr,
            Ok(false) => {}
            Err(_) => Self::abort(),
        }

        // SAFETY: alloc returns a valid pointer for new_layout or null; we only use it if non-null.
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return null_mut();
        }

        // SAFETY: new_ptr is freshly allocated for at least new_layout.size() bytes; ptr is
        // valid for old.size() bytes.
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_layout.size())) };
        // SAFETY: ptr was validated above as a pointer this allocator owns.
        unsafe { self.dealloc(ptr, old) };

        new_ptr
    }

    /// Allocates zero-initialized memory for `layout`.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw, zero-initialized memory. The caller must use it
    /// only according to `layout` and eventually pass it back to this allocator with a
    /// compatible layout.
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let Some(core) = self.core() else {
            return null_mut();
        };

        // Extents: ExtentHeap owns cache-vs-fresh zeroing (skip memset on fresh maps).
        if SizeClasses::id_for(spec).is_none() {
            // SAFETY: core is retained by this Allocator while loaded from self.core.
            let core_ref = unsafe { core.as_ref() };
            let mut state = core_ref.state().lock();
            let heap = THREAD_HEAP.with(|heap| heap.get_or_acquire(core, &mut state));
            return state
                .allocate_extent(heap, spec, core_ref.pages(), ExtentInit::Zeroed)
                .map_or(null_mut(), NonNull::as_ptr);
        }

        // Runs: blocks are reused memory; allocate then zero once at this boundary.
        // SAFETY: alloc returns a valid pointer for layout or null; we only use it if non-null.
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() {
            // SAFETY: ptr was just allocated for layout and is valid for layout.size() bytes.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }
        ptr
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
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let current_heap = THREAD_HEAP.with(|heap| heap.heap_id(core));
        let pages = core_ref.pages();
        let Some(entry) = pages.get(ptr) else {
            return Err(AllocatorError::UnknownPointer);
        };

        match entry {
            PageOwner::Run(run) => Self::dealloc_run_slow(core_ref, run, ptr, current_heap, pages),
            PageOwner::Extent(extent) => {
                Self::dealloc_extent_slow(core_ref, extent, ptr, current_heap, pages)
            }
        }
    }

    #[cold]
    fn dealloc_run_slow(
        core_ref: &AllocatorCore,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
        let heap_id = unsafe { run.as_ref() }.heap_id();

        if Some(heap_id) == current_heap {
            let mut state = core_ref.state().lock();
            return state.dealloc_run_local(run, ptr, pages);
        }

        let mode = {
            let state = core_ref.state().lock();
            let heap = state
                .heaps
                .get(heap_id)
                .ok_or(AllocatorError::InvalidMetadata)?;
            heap.mode()
        };

        match mode {
            HeapMode::Active => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { run.as_ref() }
                    .claim_free(ptr)
                    .map_err(AllocatorError::from)?;
                if let Err(error) = Self::enqueue_remote(core_ref, heap_id, ptr) {
                    // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
                    if unsafe { run.as_ref() }.unclaim(ptr).is_err() {
                        AllocatorState::abort_internal();
                    }
                    let mut state = core_ref.state().lock();
                    let heap = state
                        .heaps
                        .get(heap_id)
                        .ok_or(AllocatorError::InvalidMetadata)?;
                    if heap.is_draining() {
                        return state.dealloc_run_draining(heap_id, run, ptr, pages);
                    }
                    return Err(error);
                }
                Ok(())
            }
            HeapMode::Draining => {
                let mut state = core_ref.state().lock();
                state.dealloc_run_draining(heap_id, run, ptr, pages)
            }
            HeapMode::Free => Err(AllocatorError::InvalidMetadata),
        }
    }

    #[cold]
    fn dealloc_extent_slow(
        core_ref: &AllocatorCore,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Extent>.
        let heap_id = unsafe { extent.as_ref() }.heap_id();

        if Some(heap_id) == current_heap {
            let mut state = core_ref.state().lock();
            return state.dealloc_extent_local(extent, ptr, pages);
        }

        let mode = {
            let state = core_ref.state().lock();
            let heap = state
                .heaps
                .get(heap_id)
                .ok_or(AllocatorError::InvalidMetadata)?;
            heap.mode()
        };

        match mode {
            HeapMode::Active => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { extent.as_ref() }
                    .claim_free()
                    .map_err(AllocatorError::from)?;
                if let Err(error) = Self::enqueue_remote(core_ref, heap_id, ptr) {
                    // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                    if unsafe { extent.as_ref() }.unclaim().is_err() {
                        AllocatorState::abort_internal();
                    }
                    let mut state = core_ref.state().lock();
                    let heap = state
                        .heaps
                        .get(heap_id)
                        .ok_or(AllocatorError::InvalidMetadata)?;
                    if heap.is_draining() {
                        return state.dealloc_extent_draining(heap_id, extent, ptr, pages);
                    }
                    return Err(error);
                }
                Ok(())
            }
            HeapMode::Draining => {
                let mut state = core_ref.state().lock();
                state.dealloc_extent_draining(heap_id, extent, ptr, pages)
            }
            HeapMode::Free => Err(AllocatorError::InvalidMetadata),
        }
    }

    /// Coalesce onto the TLS remote batch; publish any returned list to its target inbox.
    fn enqueue_remote(
        core_ref: &AllocatorCore,
        heap_id: HeapId,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let pending = THREAD_HEAP.with(|thread| thread.enqueue_remote(heap_id, ptr));
        let Some((id, list)) = pending else {
            return Ok(());
        };

        let state = core_ref.state().lock();
        state
            .heaps
            .push_remote_batch(id, &list)
            .map_err(AllocatorError::from)
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
    pub(crate) fn with_config(config: AllocatorConfig) -> Self {
        Self {
            heaps: HeapTable::new(config),
        }
    }

    pub(crate) fn release_heap(
        &mut self,
        heap: HeapId,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        self.heaps.release_heap(heap, pages)?;
        Ok(())
    }

    pub(crate) fn acquire_heap(&mut self) -> Option<NonNull<Heap>> {
        self.heaps.acquire()
    }

    fn allocate(
        &mut self,
        current_heap: Option<HeapId>,
        class: Option<SizeClassId>,
        spec: LayoutSpec,
        pages: &PageMap,
    ) -> Option<NonNull<u8>> {
        let heap_id = current_heap?;
        let heap = self.heaps.get_mut(heap_id)?;
        if !heap.is_active() {
            return None;
        }

        match class {
            Some(class) => heap.allocate_run(class, pages),
            None => heap.allocate_extent(spec, pages, ExtentInit::Uninit),
        }
    }

    fn allocate_extent(
        &mut self,
        current_heap: Option<HeapId>,
        spec: LayoutSpec,
        pages: &PageMap,
        init: ExtentInit,
    ) -> Option<NonNull<u8>> {
        let heap_id = current_heap?;
        let heap = self.heaps.get_mut(heap_id)?;
        if !heap.is_active() {
            return None;
        }

        heap.allocate_extent(spec, pages, init)
    }

    pub(crate) fn dealloc_run_local(
        &mut self,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
        let heap_id = unsafe { run.as_ref() }.heap_id();
        let heap = self
            .heaps
            .get_mut(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;
        if !heap.inbox().is_empty() {
            heap.flush(pages).map_err(AllocatorError::from)?;
        }
        heap.free_run(run, ptr).map_err(AllocatorError::from)
    }

    pub(crate) fn dealloc_run_draining(
        &mut self,
        heap_id: HeapId,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        let heap = self
            .heaps
            .get_mut(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;
        heap.flush(pages).map_err(AllocatorError::from)?;
        heap.free_run(run, ptr).map_err(AllocatorError::from)?;
        let _ = self.heaps.try_reclaim_heap(heap_id);
        Ok(())
    }

    pub(crate) fn dealloc_extent_local(
        &mut self,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Extent>.
        let heap_id = unsafe { extent.as_ref() }.heap_id();
        let heap = self
            .heaps
            .get_mut(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;
        if !heap.inbox().is_empty() {
            heap.flush(pages).map_err(AllocatorError::from)?;
        }
        heap.free_extent(extent, ptr, pages)
            .map_err(AllocatorError::from)
    }

    pub(crate) fn dealloc_extent_draining(
        &mut self,
        heap_id: HeapId,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        let heap = self
            .heaps
            .get_mut(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;
        heap.flush(pages).map_err(AllocatorError::from)?;
        heap.free_extent(extent, ptr, pages)
            .map_err(AllocatorError::from)?;
        let _ = self.heaps.try_reclaim_heap(heap_id);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn abort_internal() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
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

impl From<RunError> for AllocatorError {
    fn from(error: RunError) -> Self {
        Self::from(RunHeapError::from(error))
    }
}

impl From<ExtentHeapError> for AllocatorError {
    fn from(error: ExtentHeapError) -> Self {
        match error {
            ExtentHeapError::MissingExtent => Self::MissingExtent,
            ExtentHeapError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentHeapError::InvalidMetadata => Self::InvalidMetadata,
            ExtentHeapError::DoubleFree => Self::DoubleFree,
        }
    }
}

impl From<ExtentError> for AllocatorError {
    fn from(error: ExtentError) -> Self {
        Self::from(ExtentHeapError::from(error))
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
    use super::*;

    /// Lazily-initialized core for an `Allocator` created in this test.
    fn allocator_core(allocator: &Allocator) -> &AllocatorCore {
        let core = allocator.core().unwrap();
        // SAFETY: core is retained by `allocator` for the lifetime of this borrow.
        unsafe { core.as_ref() }
    }

    fn acquire_id(core_ref: &AllocatorCore) -> HeapId {
        let mut state = core_ref.state().lock();
        // SAFETY: acquire returns a live table-resident heap.
        unsafe { state.heaps.acquire().unwrap().as_ref().id() }
    }

    fn allocate_small(core_ref: &AllocatorCore, id: HeapId, layout: Layout) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        let mut state = core_ref.state().lock();
        state
            .allocate(Some(id), SizeClasses::id_for(spec), spec, core_ref.pages())
            .unwrap()
    }

    fn allocate_extent(
        core_ref: &AllocatorCore,
        id: HeapId,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        let mut state = core_ref.state().lock();
        state
            .allocate_extent(Some(id), spec, core_ref.pages(), init)
            .unwrap()
    }

    fn run_of(core_ref: &AllocatorCore, ptr: NonNull<u8>) -> NonNull<Run> {
        let PageOwner::Run(run) = core_ref.pages().get(ptr).unwrap() else {
            panic!("expected a run-owned pointer");
        };
        run
    }

    fn extent_of(core_ref: &AllocatorCore, ptr: NonNull<u8>) -> NonNull<Extent> {
        let PageOwner::Extent(extent) = core_ref.pages().get(ptr).unwrap() else {
            panic!("expected an extent-owned pointer");
        };
        extent
    }

    #[test]
    fn allocator_reports_small_double_free() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(core_ref, id, layout);
        let run = run_of(core_ref, ptr);

        assert_eq!(
            Allocator::dealloc_run_slow(core_ref, run, ptr, Some(id), core_ref.pages()),
            Ok(())
        );
        assert_eq!(
            Allocator::dealloc_run_slow(core_ref, run, ptr, Some(id), core_ref.pages()),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_extent_free_unpublishes_page_entry() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(core_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(core_ref, ptr);

        {
            let mut state = core_ref.state().lock();
            assert_eq!(
                state.dealloc_extent_local(extent, ptr, core_ref.pages()),
                Ok(())
            );
        }

        assert!(core_ref.pages().get(ptr).is_none());
    }

    #[test]
    fn allocator_allocates_small_from_current_heap() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(core_ref, id, layout);
        let run = run_of(core_ref, ptr);

        // SAFETY: PageMap stores only live run pointers.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), id);

        let mut state = core_ref.state().lock();
        assert_eq!(state.dealloc_run_local(run, ptr, core_ref.pages()), Ok(()));
    }

    #[test]
    fn allocator_allocates_extent_from_current_heap() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(core_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(core_ref, ptr);

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        let mut state = core_ref.state().lock();
        assert_eq!(
            state.dealloc_extent_local(extent, ptr, core_ref.pages()),
            Ok(())
        );
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(core_ref, id, layout);
        let run = run_of(core_ref, ptr);

        // `current_heap: None` simulates a free arriving from a thread that does not
        // own this heap, exercising the claim -> batch -> push_remote_batch path.
        assert_eq!(
            Allocator::dealloc_run_slow(core_ref, run, ptr, None, core_ref.pages()),
            Ok(())
        );
        assert_eq!(
            Allocator::dealloc_run_slow(core_ref, run, ptr, None, core_ref.pages()),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_tracks_live_run_allocations_through_draining_reclaim() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = allocate_small(core_ref, id, layout);
        let second = allocate_small(core_ref, id, layout);
        let first_run = run_of(core_ref, first);
        let second_run = run_of(core_ref, second);

        let mut state = core_ref.state().lock();
        assert_eq!(state.release_heap(id, core_ref.pages()), Ok(()));
        assert_eq!(
            state.dealloc_run_draining(id, first_run, first, core_ref.pages()),
            Ok(())
        );
        assert_eq!(
            state.dealloc_run_draining(id, second_run, second, core_ref.pages()),
            Ok(())
        );
    }

    #[test]
    fn allocator_reuses_released_heap_after_draining_free() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let heap = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(core_ref, heap, layout);
        let run = run_of(core_ref, ptr);

        {
            let mut state = core_ref.state().lock();
            assert_eq!(state.release_heap(heap, core_ref.pages()), Ok(()));
            assert_eq!(
                state.dealloc_run_draining(heap, run, ptr, core_ref.pages()),
                Ok(())
            );
        }
        assert!(core_ref.pages().get(ptr).is_some());
        let reused = acquire_id(core_ref);
        assert_eq!(reused.index(), heap.index());
    }

    #[test]
    fn allocator_release_retains_empty_heap_run_page_entry_for_reuse() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let heap = acquire_id(core_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(core_ref, heap, layout);
        let run = run_of(core_ref, ptr);

        {
            let mut state = core_ref.state().lock();
            assert_eq!(state.dealloc_run_local(run, ptr, core_ref.pages()), Ok(()));
        }
        assert!(core_ref.pages().get(ptr).is_some());
        {
            let mut state = core_ref.state().lock();
            assert_eq!(state.release_heap(heap, core_ref.pages()), Ok(()));
        }
        assert!(core_ref.pages().get(ptr).is_some());

        let reused = acquire_id(core_ref);
        assert_eq!(reused.index(), heap.index());
        assert_ne!(reused.generation(), heap.generation());
        let reused_ptr = allocate_small(core_ref, reused, layout);
        assert_eq!(reused_ptr, ptr);
        let reused_run = run_of(core_ref, reused_ptr);
        let mut state = core_ref.state().lock();
        assert_eq!(
            state.dealloc_run_local(reused_run, reused_ptr, core_ref.pages()),
            Ok(())
        );
    }

    #[test]
    fn allocator_zeroed_large_allocation_uses_current_heap() {
        let allocator = Allocator::new();
        let core_ref = allocator_core(&allocator);
        let id = acquire_id(core_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(core_ref, id, layout, ExtentInit::Zeroed);
        // SAFETY: ptr was just allocated zeroed for layout and is valid for layout.size() bytes.
        assert!(
            unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) }
                .iter()
                .all(|&byte| byte == 0)
        );
        let extent = extent_of(core_ref, ptr);

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        let mut state = core_ref.state().lock();
        assert_eq!(
            state.dealloc_extent_local(extent, ptr, core_ref.pages()),
            Ok(())
        );
    }

    #[test]
    fn allocator_realloc_growth_uses_current_heap_extent() {
        let allocator = Allocator::new();
        let small = Layout::from_size_align(64, 8).unwrap();
        let large = Layout::from_size_align(128 * 1024, 8).unwrap();

        // SAFETY: small is a valid non-zero-size layout.
        let ptr = unsafe { allocator.alloc(small) };
        assert!(!ptr.is_null());
        // SAFETY: ptr was just allocated for small.size() bytes.
        unsafe { write_bytes(ptr, 0xab, small.size()) };

        let core_ref = allocator_core(&allocator);
        let id = unsafe { run_of(core_ref, NonNull::new(ptr).unwrap()).as_ref() }.heap_id();

        // SAFETY: ptr was returned by alloc(small) above and is not yet freed.
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let extent = extent_of(core_ref, NonNull::new(grown).unwrap());

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        // SAFETY: grown was returned by realloc above for large.
        unsafe { allocator.dealloc(grown, large) };
    }
}
