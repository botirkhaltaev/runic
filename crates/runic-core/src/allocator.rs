use core::{
    alloc::Layout,
    mem::ManuallyDrop,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::{
    config::AllocatorConfig,
    heap::extent::ExtentError,
    heap::table::inbox::RemoteList,
    heap::table::{THREAD_HEAP, ThreadFreeError, ThreadHeap},
    heap::{
        ExtentHeap, ExtentHeapError, ExtentInit, HeapDirectory, HeapError, HeapId, RunError,
        RunHeap, RunHeapError,
    },
    layout::LayoutSpec,
    memory::{Mapping, OsMemory, PageMap, PageOwner},
    size_class::{SizeClassId, SizeClasses},
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
        // `Layout` stays at the GlobalAlloc boundary; internals use `LayoutSpec`.
        let spec = LayoutSpec::from_layout(layout);
        if let Some(class) = SizeClasses::id_for(spec) {
            let Some(inner) = self.inner().or_else(|| self.init()) else {
                return null_mut();
            };
            // SAFETY: inner is retained by this Allocator while installed from self.inner.
            let inner_ref = unsafe { inner.as_ref() };
            if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class, inner_ref.pages())) {
                return ptr.as_ptr();
            }
            // Unbound / no owner-local heap: bind via the directory and allocate there.
            return Self::alloc_remote(inner, inner_ref, class);
        }

        let Some(inner) = self.inner().or_else(|| self.init()) else {
            return null_mut();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let inner_ref = unsafe { inner.as_ref() };
        if let Some(ptr) = THREAD_HEAP
            .with(|tls| tls.alloc_extent(inner, spec, inner_ref.pages(), ExtentInit::Uninit))
        {
            return ptr.as_ptr();
        }
        Self::alloc_extent_remote(inner, inner_ref, spec, ExtentInit::Uninit)
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
        // One TLS entry for lookup + owner-local free; remote/abort runs after `with`.
        let remote = THREAD_HEAP.with(|tls| {
            let Some(owner) = tls.lookup_owner(inner, inner_ref.pages(), ptr) else {
                Self::abort();
            };
            match owner {
                PageOwner::Run(run) => tls.free(inner, run, ptr).map_err(|error| (owner, error)),
                PageOwner::Extent(extent) => tls
                    .free_extent(inner, extent, ptr)
                    .map_err(|error| (owner, error)),
            }
            .err()
        });
        if let Some((owner, error)) = remote {
            Self::dealloc_remote(inner, owner, ptr, error);
        }
    }

    /// TLS free was not owner-local: `free_remote`, or abort on heap-domain errors.
    #[cold]
    #[inline(never)]
    fn dealloc_remote(
        inner: NonNull<AllocatorInner>,
        owner: PageOwner,
        ptr: NonNull<u8>,
        error: ThreadFreeError,
    ) {
        match error {
            ThreadFreeError::NotBound => {
                if Self::free_remote(inner, owner, ptr).is_err() {
                    Self::abort();
                }
            }
            ThreadFreeError::Heap(_) => Self::abort(),
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
        // One classify: small → allocate then zero; large → extent path owns zeroing.
        let spec = LayoutSpec::from_layout(layout);
        let Some(class) = SizeClasses::id_for(spec) else {
            let Some(inner) = self.inner().or_else(|| self.init()) else {
                return null_mut();
            };
            // SAFETY: inner is retained by this Allocator while installed from self.inner.
            let inner_ref = unsafe { inner.as_ref() };
            if let Some(ptr) = THREAD_HEAP
                .with(|tls| tls.alloc_extent(inner, spec, inner_ref.pages(), ExtentInit::Zeroed))
            {
                return ptr.as_ptr();
            }
            return Self::alloc_extent_remote(inner, inner_ref, spec, ExtentInit::Zeroed);
        };

        let Some(inner) = self.inner().or_else(|| self.init()) else {
            return null_mut();
        };
        // SAFETY: inner is retained by this Allocator while installed from self.inner.
        let inner_ref = unsafe { inner.as_ref() };
        let ptr =
            if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class, inner_ref.pages())) {
                ptr.as_ptr()
            } else {
                Self::alloc_remote(inner, inner_ref, class)
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

    /// Not owner-local on TLS: bind a slot via the directory and allocate a small run block.
    #[cold]
    #[inline(never)]
    fn alloc_remote(
        inner: NonNull<AllocatorInner>,
        inner_ref: &AllocatorInner,
        class: SizeClassId,
    ) -> *mut u8 {
        let heap_id = THREAD_HEAP.with(|tls| tls.bind(inner, &inner_ref.directory));
        let pages = inner_ref.pages();
        let Some(heap_id) = heap_id else {
            return null_mut();
        };
        let Some(slot) = inner_ref.directory.slot(heap_id) else {
            return null_mut();
        };
        if !slot.state().is_active() {
            return null_mut();
        }
        // SAFETY: just bound as Active TLS owner for this slot.
        unsafe { slot.alloc_run(class, pages) }.map_or(null_mut(), NonNull::as_ptr)
    }

    /// Not owner-local on TLS: bind a slot via the directory and allocate an extent.
    #[cold]
    #[inline(never)]
    fn alloc_extent_remote(
        inner: NonNull<AllocatorInner>,
        inner_ref: &AllocatorInner,
        spec: LayoutSpec,
        init: ExtentInit,
    ) -> *mut u8 {
        let heap_id = THREAD_HEAP.with(|tls| tls.bind(inner, &inner_ref.directory));
        let Some(heap_id) = heap_id else {
            return null_mut();
        };
        let Some(slot) = inner_ref.directory.slot(heap_id) else {
            return null_mut();
        };
        if !slot.state().is_active() {
            return null_mut();
        }
        // SAFETY: just bound as Active TLS owner for this slot.
        unsafe { slot.allocate_extent(spec, inner_ref.pages(), init) }
            .map_or(null_mut(), NonNull::as_ptr)
    }

    /// Cross-heap free: Active claim→batch→publish-on-flush, or Draining late free.
    ///
    /// Bound coalesce-only frees do not acquire a `PublisherLease`. Admission is only for
    /// actual inbox publication (`HeapDirectory::publish` / unbound singleton). In-flight
    /// unpublished TLS batches stay live via `RemotePending` (not the publisher count).
    #[cold]
    #[inline(never)]
    fn free_remote(
        inner: NonNull<AllocatorInner>,
        owner: PageOwner,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        // SAFETY: caller retains `inner` for the duration of this call (Allocator or TLS).
        let inner_ref = unsafe { inner.as_ref() };
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
        let pages = inner_ref.pages();
        let slot = inner_ref
            .directory
            .slot(heap_id)
            .ok_or(AllocatorError::InvalidMetadata)?;

        if !slot.state().is_active() {
            return Self::free_remote_draining(inner_ref, heap_id, owner, ptr, pages);
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

        // Bound freers coalesce via TLS batch. Never-bound freers publish a singleton
        // here (cold path) so `ThreadHeap::batch` stays unbound-free for local codegen.
        let pending = THREAD_HEAP.with(|tls| {
            if tls.is_empty() {
                Some((heap_id, RemoteList::from_ends(ptr, ptr)))
            } else {
                tls.batch(heap_id, ptr)
            }
        });
        // After a same-target flush the TLS batch is empty. Target-change leaves the new
        // claim coalesced; coalesce-only (`None`) always retains a partial batch.
        let mut may_hold_partial = pending.is_none();
        if let Some((id, list)) = pending {
            if id == heap_id {
                // Same-target flush: admit on the already-resolved slot (unbound singleton /
                // capacity). Avoid a second directory lookup on the publish hot path.
                match slot.publisher(heap_id) {
                    Ok(lease) => lease.publish(&list),
                    Err(HeapError::InvalidHeap) => {
                        inner_ref
                            .directory
                            .publish(id, &list, pages)
                            .map_err(AllocatorError::from)?;
                    }
                    Err(error) => return Err(AllocatorError::from(error)),
                }
            } else {
                may_hold_partial = true;
                inner_ref
                    .directory
                    .publish(id, &list, pages)
                    .map_err(AllocatorError::from)?;
            }
        }

        // Capacity / target-change flushes already published above. A later Active→Draining
        // close under a coalesce-only free must push the partial TLS batch immediately.
        if may_hold_partial
            && !slot.state().is_active()
            && let Some((id, list)) = THREAD_HEAP.with(ThreadHeap::take_batch)
        {
            inner_ref
                .directory
                .publish(id, &list, pages)
                .map_err(AllocatorError::from)?;
        }

        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn free_remote_draining(
        inner: &AllocatorInner,
        heap_id: HeapId,
        owner: PageOwner,
        ptr: NonNull<u8>,
        pages: &PageMap,
    ) -> Result<(), AllocatorError> {
        let mut pending = THREAD_HEAP.with(ThreadHeap::take_batch);
        while let Some((id, list)) = pending {
            inner
                .directory
                .publish(id, &list, pages)
                .map_err(AllocatorError::from)?;
            pending = THREAD_HEAP.with(ThreadHeap::take_batch);
        }
        inner
            .directory
            .free_draining(heap_id, owner, ptr, pages)
            .map_err(AllocatorError::from)
    }
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
                SizeClasses::id_for(LayoutSpec::from_layout(layout)).unwrap(),
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
        unsafe { slot.allocate_extent(spec, inner_ref.pages(), init) }.unwrap()
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
            Allocator::free_remote(inner, PageOwner::Run(run), ptr),
            Ok(())
        );
        assert_eq!(
            Allocator::free_remote(inner, PageOwner::Run(run), ptr),
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

        // Claim without publishing: RemotePending keeps the heap live so retire cannot reclaim.
        assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
        assert_eq!(inner_ref.directory.retire(id, inner_ref.pages()), Ok(()));
        assert_eq!(
            inner_ref.directory.slot(id).map(|s| s.state().mode()),
            Some(HeapMode::Draining)
        );
        let list = RemoteList::from_ends(ptr, ptr);
        assert_eq!(
            inner_ref.directory.publish(id, &list, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.directory.slot(id).is_none());
    }

    #[test]
    fn target_change_publishes_previous_batch_under_draining() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        // SAFETY: inner is retained by `allocator` for the lifetime of this test.
        let inner_ref = unsafe { inner.as_ref() };
        let first = acquire_id(inner_ref);
        let second = acquire_id(inner_ref);
        let layout = Layout::from_size_align(64, 8).unwrap();

        // Bind freer TLS so batches coalesce; unbound freers publish immediately.
        THREAD_HEAP.with(|tls| assert!(tls.bind(inner, &inner_ref.directory).is_some()));

        let ptr_a = allocate_small(inner_ref, first, layout);
        let run_a = run_of(inner_ref, ptr_a);
        assert_eq!(
            Allocator::free_remote(inner, PageOwner::Run(run_a), ptr_a),
            Ok(())
        );

        assert_eq!(inner_ref.directory.retire(first, inner_ref.pages()), Ok(()));
        assert_eq!(
            inner_ref.directory.slot(first).map(|s| s.state().mode()),
            Some(HeapMode::Draining)
        );

        let ptr_b = allocate_small(inner_ref, second, layout);
        let run_b = run_of(inner_ref, ptr_b);
        // Target change publishes the draining heap's retained batch, then retains ptr_b.
        assert_eq!(
            Allocator::free_remote(inner, PageOwner::Run(run_b), ptr_b),
            Ok(())
        );
        assert!(inner_ref.directory.slot(first).is_none());

        // Drain the freer's retained second-heap batch so TLS state does not leak across tests.
        let mut pending = None;
        THREAD_HEAP.with(|tls| pending = tls.take_batch());
        let (publish_id, list) = pending.expect("second remote free retained in TLS batch");
        assert_eq!(publish_id, second);
        assert_eq!(
            inner_ref
                .directory
                .publish(publish_id, &list, inner_ref.pages()),
            Ok(())
        );
        THREAD_HEAP.with(ThreadHeap::unbind);
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
        assert_eq!(
            inner_ref.directory.free_draining(
                id,
                PageOwner::Run(first_run),
                first,
                inner_ref.pages()
            ),
            Ok(())
        );
        assert!(inner_ref.directory.slot(id).is_some());
        assert_eq!(
            inner_ref.directory.free_draining(
                id,
                PageOwner::Run(second_run),
                second,
                inner_ref.pages()
            ),
            Ok(())
        );
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
        assert_eq!(
            inner_ref
                .directory
                .free_draining(heap, PageOwner::Run(run), ptr, inner_ref.pages()),
            Ok(())
        );
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
