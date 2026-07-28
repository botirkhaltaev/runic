use core::{
    alloc::Layout,
    mem::ManuallyDrop,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::{
    config::AllocatorConfig,
    heap::directory::{THREAD_HEAP, ThreadFreeError},
    heap::extent::ExtentError,
    heap::{ExtentInit, HeapDirectory, HeapError, RunError},
    layout::LayoutSpec,
    memory::{Mapping, OsMemory, PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

pub struct Allocator {
    config: AllocatorConfig,
    inner: AtomicPtr<AllocatorInner>,
}

/// Refcounted mmap instance for one Allocator. Not a domain entity.
///
/// Self-hosted: the value lives inside the mmap owned by `storage`. [`Drop`]
/// tears down `pages` then `directory` before `storage` munmaps that region.
pub(crate) struct AllocatorInner {
    refs: AtomicU32,
    pages: ManuallyDrop<PageMap>,
    pub(crate) directory: ManuallyDrop<HeapDirectory>,
    storage: ManuallyDrop<Mapping>,
}

impl Drop for AllocatorInner {
    fn drop(&mut self) {
        // SAFETY: Drop runs once for the final AllocatorInner reference. Order
        // is pages → directory → storage so metadata releases complete before the
        // self-hosting mmap is unmapped.
        unsafe {
            ManuallyDrop::drop(&mut self.pages);
            ManuallyDrop::drop(&mut self.directory);
            ManuallyDrop::drop(&mut self.storage);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocatorError {
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
        let Some(inner) = self.inner().or_else(|| self.init()) else {
            return null_mut();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let pages = unsafe { inner.as_ref() }.pages();
        if let Some(class) = SizeClasses::class_for(spec) {
            if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class, pages)) {
                return ptr.as_ptr();
            }
            return Self::alloc_unbound(inner, &UnboundRequest::Run(class));
        }
        if let Some(ptr) =
            THREAD_HEAP.with(|tls| tls.alloc_extent(inner, spec, pages, ExtentInit::Uninit))
        {
            return ptr.as_ptr();
        }
        Self::alloc_unbound(inner, &UnboundRequest::Extent(spec, ExtentInit::Uninit))
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
        let pages = unsafe { inner.as_ref() }.pages();
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };
        // One TLS entry for lookup + owner-local free; cross-heap/abort after `with`.
        let remote = THREAD_HEAP.with(|tls| {
            let Some(owner) = tls.lookup_owner(inner, pages, ptr) else {
                Self::abort();
            };
            // Match here (not inside ThreadHeap) so the sticky run path stays typed and lean.
            match owner {
                PageOwner::Run(run) => tls
                    .free_run(inner, run, ptr, pages)
                    .map_err(|error| (owner, error)),
                PageOwner::Extent(extent) => tls
                    .free_extent(inner, extent, ptr, pages)
                    .map_err(|error| (owner, error)),
            }
            .err()
        });
        if let Some((owner, error)) = remote {
            match error {
                ThreadFreeError::Heap(_) => Self::abort(),
                ThreadFreeError::NotBound => {
                    if Self::free_cross_heap(inner, owner, ptr).is_err() {
                        Self::abort();
                    }
                }
            }
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
        let inner = unsafe { inner.as_ref() };

        let Some(old_ptr) = NonNull::new(ptr) else {
            return null_mut();
        };
        let Some(entry) = inner.pages().get(old_ptr) else {
            Self::abort();
        };

        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return null_mut();
        };
        let new_spec = LayoutSpec::from_layout(new_layout);

        let resized = match entry {
            PageOwner::Run(run) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { run.as_ref() }
                    .resize_in_place(old_ptr, new_spec)
                    .map_err(AllocatorError::from)
            }
            PageOwner::Extent(mut extent) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arena.
                unsafe { extent.as_mut() }
                    .resize_in_place(old_ptr, new_spec)
                    .map_err(AllocatorError::from)
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
        let Some(inner) = self.inner().or_else(|| self.init()) else {
            return null_mut();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let pages = unsafe { inner.as_ref() }.pages();
        let Some(class) = SizeClasses::class_for(spec) else {
            if let Some(ptr) =
                THREAD_HEAP.with(|tls| tls.alloc_extent(inner, spec, pages, ExtentInit::Zeroed))
            {
                return ptr.as_ptr();
            }
            return Self::alloc_unbound(inner, &UnboundRequest::Extent(spec, ExtentInit::Zeroed));
        };

        let ptr = if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class, pages)) {
            ptr.as_ptr()
        } else {
            Self::alloc_unbound(inner, &UnboundRequest::Run(class))
        };
        if !ptr.is_null() {
            // SAFETY: ptr was just allocated for layout and is valid for layout.size() bytes.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }
        ptr
    }

    /// Sole process-abort sink for this crate. Other layers return domain `Result`s
    /// or call this; do not add a second `abort()` copy.
    #[cold]
    #[inline(never)]
    pub(crate) fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }

    fn inner(&self) -> Option<NonNull<AllocatorInner>> {
        NonNull::new(self.inner.load(Ordering::Acquire))
    }

    #[cold]
    #[inline(never)]
    fn init(&self) -> Option<NonNull<AllocatorInner>> {
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

    /// Not owner-local on TLS: bind Active slot, then flush-then-alloc (run or extent).
    #[cold]
    #[inline(never)]
    fn alloc_unbound(inner: NonNull<AllocatorInner>, request: &UnboundRequest) -> *mut u8 {
        let Some(heap_id) = THREAD_HEAP.with(|tls| tls.bind(inner)) else {
            return null_mut();
        };
        // SAFETY: caller retains `inner` for this cold unbound alloc.
        let inner = unsafe { inner.as_ref() };
        let Some(slot) = inner.directory.slot(heap_id) else {
            return null_mut();
        };
        if !slot.state().is_active() {
            return null_mut();
        }
        // SAFETY: just bound as Active TLS owner for this slot.
        let ptr = match *request {
            // SAFETY: Active TLS owner for `slot`.
            UnboundRequest::Run(class) => unsafe { slot.alloc_run(class, inner.pages()) },
            // SAFETY: Active TLS owner for `slot`.
            UnboundRequest::Extent(spec, init) => unsafe {
                slot.alloc_extent(spec, inner.pages(), init)
            },
        };
        ptr.map_or(null_mut(), NonNull::as_ptr)
    }

    /// Cross-heap free: Active claim → enqueue, or exclusive late free under Draining.
    ///
    /// Coalescing is by owner inbox. `NotBound` callers only — heap-domain errors abort
    /// in `dealloc` before this runs.
    #[cold]
    #[inline(never)]
    fn free_cross_heap(
        inner: NonNull<AllocatorInner>,
        owner: PageOwner,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        // SAFETY: caller retains `inner` for the duration of this call.
        let inner = unsafe { inner.as_ref() };
        let heap_id = match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arenas.
                unsafe { run.as_ref() }.heap_id()
            }
            PageOwner::Extent(extent) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arenas.
                unsafe { extent.as_ref() }.heap_id()
            }
        };
        let pages = inner.pages();
        let directory = &inner.directory;
        let slot = directory
            .slot(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;

        if !slot.state().is_active() {
            let mut locked = directory.lock(heap_id).map_err(AllocatorError::from)?;
            return locked.free(owner, ptr, pages).map_err(AllocatorError::from);
        }

        match owner {
            PageOwner::Run(run) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arenas.
                unsafe { run.as_ref() }
                    .claim(ptr)
                    .map_err(AllocatorError::from)?;
            }
            PageOwner::Extent(extent) => {
                // SAFETY: PageMap stores only pointers published from this allocator's live arenas.
                unsafe { extent.as_ref() }
                    .claim(ptr)
                    .map_err(AllocatorError::from)?;
            }
        }

        match slot.enqueue(heap_id, owner) {
            Ok(()) => Ok(()),
            Err(HeapError::InvalidHeap) => {
                let mut locked = directory.lock(heap_id).map_err(AllocatorError::from)?;
                locked.enqueue(owner);
                locked.flush(pages).map_err(AllocatorError::from)
            }
            Err(error) => Err(AllocatorError::from(error)),
        }
    }
}

/// Cold unbound alloc request — one bind/Active path for run and extent.
#[derive(Clone, Copy)]
enum UnboundRequest {
    Run(SizeClass),
    Extent(LayoutSpec, ExtentInit),
}

impl AllocatorInner {
    fn new(config: AllocatorConfig) -> Option<NonNull<Self>> {
        let storage = OsMemory::map(core::mem::size_of::<Self>())?;
        let inner = storage.base().cast::<Self>();

        // SAFETY: inner points to uniquely owned mmap storage aligned at least to a page boundary.
        // `storage` is moved into the value it backs; [`Drop`] unmaps it only after pages/directory.
        unsafe {
            inner.as_ptr().write(Self {
                refs: AtomicU32::new(1),
                pages: ManuallyDrop::new(PageMap::new()),
                directory: ManuallyDrop::new(HeapDirectory::new(config)),
                storage: ManuallyDrop::new(storage),
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
                Allocator::abort();
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
                Allocator::abort();
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

    pub(crate) fn pages(&self) -> &PageMap {
        &self.pages
    }

    unsafe fn destroy(inner: NonNull<Self>) {
        // SAFETY: caller guarantees this is the final reference to inner.
        // [`Drop`] drops pages → directory → storage; nothing touches `inner` afterward.
        unsafe { inner.as_ptr().drop_in_place() };
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

impl From<RunError> for AllocatorError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidPointer => Self::InvalidRunPointer,
            RunError::DoubleFree => Self::DoubleFree,
        }
    }
}

impl From<ExtentError> for AllocatorError {
    fn from(error: ExtentError) -> Self {
        match error {
            ExtentError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentError::DoubleFree => Self::DoubleFree,
        }
    }
}

impl From<HeapError> for AllocatorError {
    fn from(error: HeapError) -> Self {
        match error {
            HeapError::InvalidHeap | HeapError::InvalidMetadata => Self::InvalidMetadata,
            HeapError::InvalidRunPointer => Self::InvalidRunPointer,
            HeapError::InvalidExtentPointer => Self::InvalidExtentPointer,
            HeapError::DoubleFree => Self::DoubleFree,
            HeapError::MissingExtent => Self::MissingExtent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{Extent, HeapId, HeapMode, Run};

    /// Lazily-initialized inner for an `Allocator` created in this test.
    fn allocator_inner_ptr(allocator: &Allocator) -> NonNull<AllocatorInner> {
        allocator.inner().or_else(|| allocator.init()).unwrap()
    }

    fn allocator_inner(allocator: &Allocator) -> &AllocatorInner {
        // SAFETY: inner is retained by `allocator` for the lifetime of this borrow.
        unsafe { allocator_inner_ptr(allocator).as_ref() }
    }

    fn acquire_id(inner_ref: &AllocatorInner) -> HeapId {
        inner_ref.directory.acquire().unwrap().0
    }

    fn allocate_small(inner_ref: &AllocatorInner, id: HeapId, layout: Layout) -> NonNull<u8> {
        let slot = inner_ref.directory.slot(id).unwrap();
        assert!(slot.state().is_active());
        // SAFETY: test drives Active slot exclusively.
        unsafe {
            slot.alloc_run(
                SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap(),
                inner_ref.pages(),
            )
        }
        .unwrap()
    }

    fn allocate_extent(
        inner_ref: &AllocatorInner,
        id: HeapId,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        let slot = inner_ref.directory.slot(id).unwrap();
        assert!(slot.state().is_active());
        // SAFETY: test drives Active slot exclusively.
        unsafe { slot.alloc_extent(spec, inner_ref.pages(), init) }.unwrap()
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

    fn free_owner(inner_ref: &AllocatorInner, id: HeapId, owner: PageOwner, ptr: NonNull<u8>) {
        let slot = inner_ref.directory.slot(id).unwrap();
        // SAFETY: test drives Active/Draining slot exclusively.
        assert_eq!(unsafe { slot.free(owner, ptr, inner_ref.pages()) }, Ok(()));
    }

    #[test]
    fn allocator_reports_small_double_free() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);
        let pages = inner_ref.pages();
        let slot = inner_ref.directory.slot(id).unwrap();

        // SAFETY: test drives Active slot exclusively.
        assert_eq!(
            unsafe { slot.free(PageOwner::Run(run), ptr, pages) },
            Ok(())
        );
        assert_eq!(
            unsafe { slot.free(PageOwner::Run(run), ptr, pages) },
            Err(HeapError::DoubleFree)
        );
    }

    #[test]
    fn allocator_extent_free_keeps_page_entry_while_cached() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(inner_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(inner_ref, ptr);

        free_owner(inner_ref, id, PageOwner::Extent(extent), ptr);
        assert_eq!(inner_ref.pages().get(ptr), Some(PageOwner::Extent(extent)));
    }

    #[test]
    fn allocator_extent_free_unpublishes_when_drop_policy() {
        let allocator = Allocator::with_config(
            AllocatorConfig::new().with_extent_policy(crate::config::ExtentPolicy::Drop),
        );
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let ptr = allocate_extent(inner_ref, id, layout, ExtentInit::Uninit);
        let extent = extent_of(inner_ref, ptr);

        free_owner(inner_ref, id, PageOwner::Extent(extent), ptr);
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
        free_owner(inner_ref, id, PageOwner::Run(run), ptr);
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
        free_owner(inner_ref, id, PageOwner::Extent(extent), ptr);
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        // SAFETY: inner is retained by `allocator` for the lifetime of this test.
        let inner_ref = unsafe { inner.as_ref() };
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        // Unbound TLS simulates a free from a thread that does not own this heap.
        assert_eq!(
            Allocator::free_cross_heap(inner, PageOwner::Run(run), ptr),
            Ok(())
        );
        assert_eq!(
            Allocator::free_cross_heap(inner, PageOwner::Run(run), ptr),
            Err(AllocatorError::DoubleFree)
        );
    }

    #[test]
    fn retained_remote_claim_completes_under_draining() {
        use crate::heap::directory::inbox::InboxNode;

        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let id = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, id, layout);
        let run = run_of(inner_ref, ptr);

        // Claim without enqueueing: the outstanding claim keeps the heap live so retire
        // cannot reclaim until the run is accepted.
        assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
        assert_eq!(inner_ref.directory.retire(id, inner_ref.pages()), Ok(()));
        assert_eq!(
            inner_ref.directory.slot(id).map(|s| s.state().mode()),
            Some(HeapMode::Draining)
        );
        // SAFETY: run is a live arena run for this slot.
        assert!(unsafe { run.as_ref() }.link().try_queue());
        let mut locked = inner_ref.directory.lock(id).unwrap();
        locked.enqueue(PageOwner::Run(run));
        assert_eq!(locked.flush(inner_ref.pages()), Ok(()));
        drop(locked);
        assert!(inner_ref.directory.slot(id).is_none());
    }

    #[test]
    fn remote_frees_to_distinct_heaps_publish_independently_without_batching() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        // SAFETY: inner is retained by `allocator` for the lifetime of this test.
        let inner_ref = unsafe { inner.as_ref() };
        let first = acquire_id(inner_ref);
        let second = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();

        let ptr_a = allocate_small(inner_ref, first, layout);
        let run_a = run_of(inner_ref, ptr_a);
        let ptr_b = allocate_small(inner_ref, second, layout);
        let run_b = run_of(inner_ref, ptr_b);

        // Each remote free claims and enqueues its own target immediately — no per-thread
        // batch retains one heap's claim while a different heap's free is in flight.
        assert_eq!(
            Allocator::free_cross_heap(inner, PageOwner::Run(run_a), ptr_a),
            Ok(())
        );
        assert_eq!(
            Allocator::free_cross_heap(inner, PageOwner::Run(run_b), ptr_b),
            Ok(())
        );

        // SAFETY: test drives Active slot exclusively; the claim above already enqueued.
        unsafe {
            inner_ref
                .directory
                .slot(first)
                .unwrap()
                .flush(inner_ref.pages())
        }
        .unwrap();
        // SAFETY: same contract.
        unsafe {
            inner_ref
                .directory
                .slot(second)
                .unwrap()
                .flush(inner_ref.pages())
        }
        .unwrap();
        assert!(!unsafe { run_a.as_ref() }.is_live());
        assert!(!unsafe { run_b.as_ref() }.is_live());
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

        assert_eq!(inner_ref.directory.retire(id, inner_ref.pages()), Ok(()));
        {
            let mut locked = inner_ref.directory.lock(id).unwrap();
            assert_eq!(
                locked.free(PageOwner::Run(first_run), first, inner_ref.pages()),
                Ok(())
            );
        }
        assert!(inner_ref.directory.slot(id).is_some());
        {
            let mut locked = inner_ref.directory.lock(id).unwrap();
            assert_eq!(
                locked.free(PageOwner::Run(second_run), second, inner_ref.pages()),
                Ok(())
            );
        }
        assert!(inner_ref.directory.slot(id).is_none());
    }

    #[test]
    fn allocator_reuses_released_heap_after_draining_free() {
        let allocator = Allocator::new();
        let inner_ref = allocator_inner(&allocator);
        let heap = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ptr = allocate_small(inner_ref, heap, layout);
        let run = run_of(inner_ref, ptr);

        assert_eq!(inner_ref.directory.retire(heap, inner_ref.pages()), Ok(()));
        {
            let mut locked = inner_ref.directory.lock(heap).unwrap();
            assert_eq!(
                locked.free(PageOwner::Run(run), ptr, inner_ref.pages()),
                Ok(())
            );
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

        free_owner(inner_ref, heap, PageOwner::Run(run), ptr);
        assert!(inner_ref.pages().get(ptr).is_some());
        assert_eq!(inner_ref.directory.retire(heap, inner_ref.pages()), Ok(()));
        assert!(inner_ref.pages().get(ptr).is_some());

        let reused = acquire_id(inner_ref);
        assert_eq!(reused.index(), heap.index());
        assert_ne!(reused.generation(), heap.generation());
        let reused_ptr = allocate_small(inner_ref, reused, layout);
        assert_eq!(reused_ptr, ptr);
        let reused_run = run_of(inner_ref, reused_ptr);
        free_owner(inner_ref, reused, PageOwner::Run(reused_run), reused_ptr);
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
        free_owner(inner_ref, id, PageOwner::Extent(extent), ptr);
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
