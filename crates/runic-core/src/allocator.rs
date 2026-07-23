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
        Extent, ExtentHeap, ExtentHeapError, ExtentInit, HeapError, HeapId, HeapMode, HeapTable,
        Run, RunError, RunHeap, RunHeapError, THREAD_HEAP,
    },
    layout::LayoutSpec,
    memory::{OsMemory, PageMap, PageOwner},
    size_class::SizeClasses,
};

pub struct Allocator {
    config: AllocatorConfig,
    inner: AtomicPtr<AllocatorInner>,
}

/// Refcounted mmap instance for one Allocator. Not a domain entity.
pub(crate) struct AllocatorInner {
    refs: AtomicU32,
    pages: PageMap,
    pub(crate) table: Mutex<HeapTable>,
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
            inner: AtomicPtr::new(core::ptr::null_mut()),
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
        let Some(inner) = self.ensure_inner() else {
            return null_mut();
        };

        let class = SizeClasses::id_for(spec);
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let inner_ref = unsafe { inner.as_ref() };
        if let Some(class) = class
            && let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class, inner_ref.pages()))
        {
            return ptr.as_ptr();
        }

        let mut table = inner_ref.table.lock();
        let heap_id = THREAD_HEAP.with(|tls| tls.bind(inner, &mut table));
        let pages = inner_ref.pages();
        let Some(heap_id) = heap_id else {
            return null_mut();
        };
        let Some(heap) = table.heap_mut(heap_id) else {
            return null_mut();
        };
        if !heap.is_active() {
            return null_mut();
        }
        match class {
            Some(class) => heap.alloc_run(class, pages),
            None => heap.allocate_extent(spec, pages, ExtentInit::Uninit),
        }
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

        let Some(inner) = self.inner() else {
            Self::abort();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let inner_ref = unsafe { inner.as_ref() };

        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };

        let Some(entry) = inner_ref.pages().get(ptr) else {
            Self::abort();
        };

        if let PageOwner::Run(run) = entry {
            // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
            let heap_id = unsafe { run.as_ref() }.heap_id();
            match THREAD_HEAP.with(|tls| tls.free(inner, heap_id, run, ptr)) {
                Ok(true) => return,
                Ok(false) => {}
                Err(_) => Self::abort(),
            }
        }

        if Self::dealloc_slow(inner, inner_ref, ptr).is_err() {
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

        let Some(inner) = self.inner() else {
            Self::abort();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let inner_ref = unsafe { inner.as_ref() };

        let Some(old_ptr) = NonNull::new(ptr) else {
            return null_mut();
        };
        let Some(entry) = inner_ref.pages().get(old_ptr) else {
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
        let Some(inner) = self.ensure_inner() else {
            return null_mut();
        };

        // Extents: ExtentHeap owns cache-vs-fresh zeroing (skip memset on fresh maps).
        if SizeClasses::id_for(spec).is_none() {
            // SAFETY: inner is retained by this Allocator while installed from self.inner.
            let inner_ref = unsafe { inner.as_ref() };
            let mut table = inner_ref.table.lock();
            let heap_id = THREAD_HEAP.with(|tls| tls.bind(inner, &mut table));
            let Some(heap_id) = heap_id else {
                return null_mut();
            };
            let Some(heap) = table.heap_mut(heap_id) else {
                return null_mut();
            };
            if !heap.is_active() {
                return null_mut();
            }
            return heap
                .allocate_extent(spec, inner_ref.pages(), ExtentInit::Zeroed)
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

    fn ensure_inner(&self) -> Option<NonNull<AllocatorInner>> {
        if let Some(inner) = self.inner() {
            return Some(inner);
        }

        self.init_inner()
    }

    #[cold]
    #[inline(never)]
    fn init_inner(&self) -> Option<NonNull<AllocatorInner>> {
        let inner = AllocatorInner::new(self.config)?;
        match self.inner.compare_exchange(
            core::ptr::null_mut(),
            inner.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(inner),
            Err(existing) => {
                AllocatorInner::release(inner);
                NonNull::new(existing)
            }
        }
    }

    fn inner(&self) -> Option<NonNull<AllocatorInner>> {
        NonNull::new(self.inner.load(Ordering::Acquire))
    }

    #[cold]
    #[inline(never)]
    fn dealloc_slow(
        inner: NonNull<AllocatorInner>,
        inner_ref: &AllocatorInner,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let current_heap = THREAD_HEAP.with(|tls| tls.bound(inner));
        let pages = inner_ref.pages();
        let Some(entry) = pages.get(ptr) else {
            return Err(AllocatorError::UnknownPointer);
        };

        match entry {
            PageOwner::Run(run) => Self::dealloc_run_slow(inner_ref, run, ptr, current_heap, pages),
            PageOwner::Extent(extent) => {
                Self::dealloc_extent_slow(inner_ref, extent, ptr, current_heap, pages)
            }
        }
    }

    #[cold]
    fn dealloc_run_slow(
        inner_ref: &AllocatorInner,
        run: NonNull<Run>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Run>.
        let heap_id = unsafe { run.as_ref() }.heap_id();

        if Some(heap_id) == current_heap {
            let mut table = inner_ref.table.lock();
            let heap = table
                .heap_mut(heap_id)
                .ok_or(AllocatorError::InvalidMetadata)?;
            return heap
                .free_run_owner(run, ptr, pages)
                .map_err(AllocatorError::from);
        }

        let mode = {
            let table = inner_ref.table.lock();
            table.mode(heap_id).ok_or(AllocatorError::InvalidMetadata)?
        };

        match mode {
            HeapMode::Active => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { run.as_ref() }
                    .claim_free(ptr)
                    .map_err(AllocatorError::from)?;
                // Re-check after claim: Pending keeps the heap live, but the slot may still
                // have gone Free/stale if the HeapId is wrong.
                {
                    let table = inner_ref.table.lock();
                    match table.mode(heap_id) {
                        Some(HeapMode::Active | HeapMode::Draining) => {}
                        Some(HeapMode::Free) | None => {
                            // SAFETY: we just claimed this block on the live PageMap run.
                            if unsafe { run.as_ref() }.unclaim(ptr).is_err() {
                                Self::abort();
                            }
                            return Err(AllocatorError::InvalidMetadata);
                        }
                    }
                }
                // Publish is mode-aware: Active → inbox, Draining → complete under lock.
                // A returned list may be a displaced previous batch; never unclaim `ptr` here.
                Self::publish_remote(inner_ref, heap_id, ptr)
            }
            HeapMode::Draining => {
                let mut table = inner_ref.table.lock();
                let heap = table
                    .heap_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                heap.flush(pages).map_err(AllocatorError::from)?;
                heap.free_run(run, ptr).map_err(AllocatorError::from)?;
                let _ = table.reclaim(heap_id);
                Ok(())
            }
            HeapMode::Free => Err(AllocatorError::InvalidMetadata),
        }
    }

    #[cold]
    fn dealloc_extent_slow(
        inner_ref: &AllocatorInner,
        extent: NonNull<Extent>,
        ptr: NonNull<u8>,
        current_heap: Option<HeapId>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        // SAFETY: PageMap stores only pointers published from this allocator's live Arena<Extent>.
        let heap_id = unsafe { extent.as_ref() }.heap_id();

        if Some(heap_id) == current_heap {
            let mut table = inner_ref.table.lock();
            let heap = table
                .heap_mut(heap_id)
                .ok_or(AllocatorError::InvalidMetadata)?;
            return heap
                .free_extent_owner(extent, ptr, pages)
                .map_err(AllocatorError::from);
        }

        let mode = {
            let table = inner_ref.table.lock();
            table.mode(heap_id).ok_or(AllocatorError::InvalidMetadata)?
        };

        match mode {
            HeapMode::Active => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { extent.as_ref() }
                    .claim_free()
                    .map_err(AllocatorError::from)?;
                {
                    let table = inner_ref.table.lock();
                    match table.mode(heap_id) {
                        Some(HeapMode::Active | HeapMode::Draining) => {}
                        Some(HeapMode::Free) | None => {
                            // SAFETY: we just claimed this extent on the live PageMap entry.
                            if unsafe { extent.as_ref() }.unclaim().is_err() {
                                Self::abort();
                            }
                            return Err(AllocatorError::InvalidMetadata);
                        }
                    }
                }
                // Publish is mode-aware: Active → inbox, Draining → complete under lock.
                // A returned list may be a displaced previous batch; never unclaim `ptr` here.
                Self::publish_remote(inner_ref, heap_id, ptr)
            }
            HeapMode::Draining => {
                let mut table = inner_ref.table.lock();
                let heap = table
                    .heap_mut(heap_id)
                    .ok_or(AllocatorError::InvalidMetadata)?;
                heap.flush(pages).map_err(AllocatorError::from)?;
                heap.free_extent(extent, ptr, pages)
                    .map_err(AllocatorError::from)?;
                let _ = table.reclaim(heap_id);
                Ok(())
            }
            HeapMode::Free => Err(AllocatorError::InvalidMetadata),
        }
    }

    /// Coalesce onto the TLS remote batch; publish any returned list (Active or Draining).
    fn publish_remote(
        inner_ref: &AllocatorInner,
        heap_id: HeapId,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let pending = THREAD_HEAP.with(|tls| tls.batch(heap_id, ptr));
        let Some((id, list)) = pending else {
            return Ok(());
        };

        let mut table = inner_ref.table.lock();
        table
            .publish(id, &list, inner_ref.pages())
            .map_err(AllocatorError::from)
    }
}

impl AllocatorInner {
    fn new(config: AllocatorConfig) -> Option<NonNull<Self>> {
        let mapping = OsMemory::map(core::mem::size_of::<Self>())?;
        let inner = mapping.base().cast::<Self>();
        core::mem::forget(mapping);

        // SAFETY: inner points to uniquely owned mmap storage aligned at least to a page boundary.
        unsafe {
            inner.as_ptr().write(Self {
                refs: AtomicU32::new(1),
                pages: PageMap::new(),
                table: Mutex::new(HeapTable::new(config)),
            });
        }

        Some(inner)
    }

    pub(crate) fn retain(inner: NonNull<Self>) -> bool {
        // SAFETY: callers obtain inner from an Allocator or an existing retained TLS entry.
        let refs = unsafe { &inner.as_ref().refs };
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

    pub(crate) fn release(inner: NonNull<Self>) {
        // SAFETY: callers release one previously retained reference to this live inner.
        let refs = unsafe { &inner.as_ref().refs };
        let mut current = refs.load(Ordering::Acquire);

        loop {
            if current == 0 {
                Self::abort();
            }

            let next = current - 1;
            match refs.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if next == 0 {
                        // SAFETY: this was the final reference, so no thread can access inner after this point.
                        unsafe { Self::destroy(inner) };
                    }
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) const fn pages(&self) -> &PageMap {
        &self.pages
    }

    unsafe fn destroy(inner: NonNull<Self>) {
        let Some(mapping_len) = OsMemory::round_to_page(core::mem::size_of::<Self>()) else {
            Self::abort();
        };
        // SAFETY: caller guarantees this is the final reference to inner.
        unsafe { inner.as_ptr().drop_in_place() };
        // SAFETY: storage was allocated by OsMemory::map and is no longer occupied by AllocatorInner.
        unsafe { OsMemory::unmap(inner.cast::<u8>(), mapping_len) };
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        let core = self.inner.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if let Some(inner) = NonNull::new(core) {
            AllocatorInner::release(inner);
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
    use crate::heap::table::inbox::RemoteList;

    /// Lazily-initialized inner for an `Allocator` created in this test.
    fn allocator_inner(allocator: &Allocator) -> &AllocatorInner {
        let inner = allocator.ensure_inner().unwrap();
        // SAFETY: inner is retained by `allocator` for the lifetime of this borrow.
        unsafe { inner.as_ref() }
    }

    fn acquire_id(inner_ref: &AllocatorInner) -> HeapId {
        let mut table = inner_ref.table.lock();
        table.acquire().unwrap().0
    }

    fn allocate_small(inner_ref: &AllocatorInner, id: HeapId, layout: Layout) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        let mut table = inner_ref.table.lock();
        let heap = table.heap_mut(id).unwrap();
        assert!(heap.is_active());
        heap.alloc_run(SizeClasses::id_for(spec).unwrap(), inner_ref.pages())
            .unwrap()
    }

    fn allocate_extent(
        inner_ref: &AllocatorInner,
        id: HeapId,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        let mut table = inner_ref.table.lock();
        let heap = table.heap_mut(id).unwrap();
        assert!(heap.is_active());
        heap.allocate_extent(spec, inner_ref.pages(), init).unwrap()
    }

    fn run_of(inner_ref: &AllocatorInner, ptr: NonNull<u8>) -> NonNull<Run> {
        let PageOwner::Run(run) = inner_ref.pages().get(ptr).unwrap() else {
            panic!("expected a run-owned pointer");
        };
        run
    }

    fn extent_of(inner_ref: &AllocatorInner, ptr: NonNull<u8>) -> NonNull<Extent> {
        let PageOwner::Extent(extent) = inner_ref.pages().get(ptr).unwrap() else {
            panic!("expected an extent-owned pointer");
        };
        extent
    }

    #[test]
    fn allocator_reports_small_double_free() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run, ptr, Some(id), inner_ref.pages()),
            Ok(())
        );
        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run, ptr, Some(id), inner_ref.pages()),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn allocator_extent_free_unpublishes_page_entry() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(inner_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(inner_ref, ptr);

        {
            let mut table = inner_ref.table.lock();
            let heap = table.heap_mut(id).unwrap();
            assert_eq!(
                heap.free_extent_owner(extent, ptr, inner_ref.pages()),
                Ok(())
            );
        }

        assert!(inner_ref.pages().get(ptr).is_none());
    }

    #[test]
    fn allocator_allocates_small_from_current_heap() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        // SAFETY: PageMap stores only live run pointers.
        assert_eq!(unsafe { run.as_ref() }.heap_id(), id);

        let mut table = inner_ref.table.lock();
        let heap = table.heap_mut(unsafe { run.as_ref() }.heap_id()).unwrap();
        assert_eq!(heap.free_run_owner(run, ptr, inner_ref.pages()), Ok(()));
    }

    #[test]
    fn allocator_allocates_extent_from_current_heap() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(inner_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(inner_ref, ptr);

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        let mut table = inner_ref.table.lock();
        let heap = table.heap_mut(id).unwrap();
        assert_eq!(
            heap.free_extent_owner(extent, ptr, inner_ref.pages()),
            Ok(())
        );
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        // `current_heap: None` simulates a free arriving from a thread that does not
        // own this heap, exercising the claim -> batch -> publish path.
        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run, ptr, None, inner_ref.pages()),
            Ok(())
        );
        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run, ptr, None, inner_ref.pages()),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn retained_remote_batch_completes_under_draining() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        // Claim without publishing, then drain the owner — late publish must complete.
        assert_eq!(unsafe { run.as_ref() }.claim_free(ptr), Ok(()));

        {
            let mut table = inner_ref.table.lock();
            assert_eq!(table.retire(id, inner_ref.pages()), Ok(()));
            assert_eq!(table.mode(id), Some(HeapMode::Draining));
            let list = RemoteList::from_ends(ptr, ptr);
            assert_eq!(table.publish(id, &list, inner_ref.pages()), Ok(()));
            assert!(table.heap(id).is_none());
        }
    }

    #[test]
    fn target_change_publishes_previous_batch_under_draining() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let first = acquire_id(inner_ref);
        let second = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();

        let ptr_a = allocate_small(inner_ref, first, layout);
        let run_a = run_of(inner_ref, ptr_a);
        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run_a, ptr_a, None, inner_ref.pages()),
            Ok(())
        );

        {
            let mut table = inner_ref.table.lock();
            assert_eq!(table.retire(first, inner_ref.pages()), Ok(()));
            assert_eq!(table.mode(first), Some(HeapMode::Draining));
        }

        let ptr_b = allocate_small(inner_ref, second, layout);
        let run_b = run_of(inner_ref, ptr_b);
        // Target change publishes the draining heap's retained batch, then retains ptr_b.
        assert_eq!(
            Allocator::dealloc_run_slow(inner_ref, run_b, ptr_b, None, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.table.lock().heap(first).is_none());

        // Drain the freer's retained second-heap batch so TLS state does not leak across tests.
        let mut pending = None;
        THREAD_HEAP.with(|tls| pending = tls.take_batch());
        let (publish_id, list) = pending.expect("second remote free retained in TLS batch");
        assert_eq!(publish_id, second);
        let mut table = inner_ref.table.lock();
        assert_eq!(table.publish(publish_id, &list, inner_ref.pages()), Ok(()));
    }

    #[test]
    fn allocator_tracks_live_run_allocations_through_draining_reclaim() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = allocate_small(inner_ref, id, layout);
        let second = allocate_small(inner_ref, id, layout);
        let first_run = run_of(inner_ref, first);
        let second_run = run_of(inner_ref, second);

        let mut table = inner_ref.table.lock();
        assert_eq!(table.retire(id, inner_ref.pages()), Ok(()));
        {
            let owner = table.heap_mut(id).unwrap();
            assert_eq!(owner.flush(inner_ref.pages()), Ok(()));
            assert_eq!(owner.free_run(first_run, first), Ok(()));
        }
        let _ = table.reclaim(id);
        {
            let owner = table.heap_mut(id).unwrap();
            assert_eq!(owner.flush(inner_ref.pages()), Ok(()));
            assert_eq!(owner.free_run(second_run, second), Ok(()));
        }
        let _ = table.reclaim(id);
    }

    #[test]
    fn allocator_reuses_released_heap_after_draining_free() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let heap = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, heap, layout);
        let run = run_of(inner_ref, ptr);

        {
            let mut table = inner_ref.table.lock();
            assert_eq!(table.retire(heap, inner_ref.pages()), Ok(()));
            {
                let owner = table.heap_mut(heap).unwrap();
                assert_eq!(owner.flush(inner_ref.pages()), Ok(()));
                assert_eq!(owner.free_run(run, ptr), Ok(()));
            }
            let _ = table.reclaim(heap);
        }
        assert!(inner_ref.pages().get(ptr).is_some());
        let reused = acquire_id(inner_ref);
        assert_eq!(reused.index(), heap.index());
    }

    #[test]
    fn allocator_release_retains_empty_heap_run_page_entry_for_reuse() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let heap = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, heap, layout);
        let run = run_of(inner_ref, ptr);

        {
            let mut table = inner_ref.table.lock();
            let heap = table.heap_mut(unsafe { run.as_ref() }.heap_id()).unwrap();
            assert_eq!(heap.free_run_owner(run, ptr, inner_ref.pages()), Ok(()));
        }
        assert!(inner_ref.pages().get(ptr).is_some());
        {
            let mut table = inner_ref.table.lock();
            assert_eq!(table.retire(heap, inner_ref.pages()), Ok(()));
        }
        assert!(inner_ref.pages().get(ptr).is_some());

        let reused = acquire_id(inner_ref);
        assert_eq!(reused.index(), heap.index());
        assert_ne!(reused.generation(), heap.generation());
        let reused_ptr = allocate_small(inner_ref, reused, layout);
        assert_eq!(reused_ptr, ptr);
        let reused_run = run_of(inner_ref, reused_ptr);
        let mut table = inner_ref.table.lock();
        assert_eq!(
            table.heap_mut(reused).unwrap().free_run_owner(
                reused_run,
                reused_ptr,
                inner_ref.pages()
            ),
            Ok(())
        );
    }

    #[test]
    fn allocator_zeroed_large_allocation_uses_current_heap() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(inner_ref, id, layout, ExtentInit::Zeroed);
        // SAFETY: ptr was just allocated zeroed for layout and is valid for layout.size() bytes.
        assert!(
            unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) }
                .iter()
                .all(|&byte| byte == 0)
        );
        let extent = extent_of(inner_ref, ptr);

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        let mut table = inner_ref.table.lock();
        let owner = table.heap_mut(id).unwrap();
        assert_eq!(
            owner.free_extent_owner(extent, ptr, inner_ref.pages()),
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

        let inner_ref = allocator_inner(&allocator);
        let id = unsafe { run_of(inner_ref, NonNull::new(ptr).unwrap()).as_ref() }.heap_id();

        // SAFETY: ptr was returned by alloc(small) above and is not yet freed.
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let extent = extent_of(inner_ref, NonNull::new(grown).unwrap());

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        // SAFETY: grown was returned by realloc above for large.
        unsafe { allocator.dealloc(grown, large) };
    }
}
