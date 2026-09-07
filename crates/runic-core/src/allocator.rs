use core::{
    alloc::Layout,
    mem::ManuallyDrop,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::{
    config::AllocatorConfig,
    heap::extent::ExtentError,
    heap::{ExtentInit, HeapError, Heaps, RunError, THREAD_HEAP, ThreadFreeError},
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
/// tears down `pages` then `heaps` before `storage` munmaps that region.
pub(crate) struct AllocatorInner {
    refs: AtomicU32,
    pages: ManuallyDrop<PageMap>,
    pub(crate) heaps: ManuallyDrop<Heaps>,
    storage: ManuallyDrop<Mapping>,
}

impl Drop for AllocatorInner {
    fn drop(&mut self) {
        // SAFETY: Drop runs once for the final AllocatorInner reference. Order
        // is pages → heaps → storage so metadata releases complete before the
        // self-hosting mmap is unmapped.
        unsafe {
            ManuallyDrop::drop(&mut self.pages);
            ManuallyDrop::drop(&mut self.heaps);
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
    #[inline]
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let inner = self.inner.load(Ordering::Acquire);
        if let Some(class) = SizeClasses::class_for(spec) {
            if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class)) {
                return ptr.as_ptr();
            }
            return self.alloc_miss(inner, class, layout);
        }
        if let Some(inner) = NonNull::new(inner) {
            return Self::alloc_extent(inner, spec, ExtentInit::Uninit);
        }
        self.alloc_uninit(layout)
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `layout`. Passing an unknown pointer, an interior pointer, or an
    /// incompatible layout violates the allocator contract and may abort.
    #[inline]
    pub unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };
        let inner = self.inner.load(Ordering::Acquire);
        if let Some(run) = THREAD_HEAP.with(|tls| tls.cached_run(inner, ptr)) {
            // SAFETY: `cached_run` returns a live own-heap run.
            match unsafe { run.as_ref().free(ptr) } {
                Ok(false) => {}
                Ok(true) => THREAD_HEAP.with(|tls| tls.push_available(run)),
                Err(_) => Self::abort(),
            }
            return;
        }
        Self::dealloc_slow(inner, ptr);
    }

    /// Changes the size of an allocation using allocate-copy-free semantics.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `old`. If a non-null pointer is supplied, no other live reference may
    /// be used to access the old allocation after successful reallocation.
    #[inline]
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
    #[inline]
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let inner = self.inner.load(Ordering::Acquire);
        let Some(class) = SizeClasses::class_for(spec) else {
            if let Some(inner) = NonNull::new(inner) {
                return Self::alloc_extent(inner, spec, ExtentInit::Zeroed);
            }
            if self.init().is_none() {
                return null_mut();
            }
            // SAFETY: inner is installed; same contract as the public method.
            return unsafe { self.alloc_zeroed(layout) };
        };

        let ptr = if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc(inner, class)) {
            ptr.as_ptr()
        } else {
            self.alloc_miss(inner, class, layout)
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

    #[inline]
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

    /// Not owner-local on TLS: bind Active heap, then flush-then-alloc (run or extent).
    #[cold]
    #[inline(never)]
    fn bind_alloc(inner: NonNull<AllocatorInner>, request: AllocKind) -> *mut u8 {
        if THREAD_HEAP.with(|tls| tls.bind(inner)).is_none() {
            return null_mut();
        }
        // SAFETY: caller retains `inner` for this cold unbound alloc.
        let pages = unsafe { inner.as_ref() }.pages();
        let ptr = match request {
            AllocKind::Run(class) => THREAD_HEAP.with(|tls| tls.alloc_after_bind(inner, class)),
            AllocKind::Extent(spec, init) => {
                THREAD_HEAP.with(|tls| tls.alloc_extent_after_bind(inner, spec, pages, init))
            }
        };
        ptr.map_or(null_mut(), NonNull::as_ptr)
    }

    /// Cross-heap free: Active claim → enqueue, or `Heaps::free` under Draining.
    ///
    /// Coalescing is by owner inbox. `Remote` callers only — heap-domain errors abort
    /// in `dealloc` before this runs.
    #[cold]
    #[inline(never)]
    fn free_remote(
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
        let heaps = &inner.heaps;
        let heap = heaps.get(heap_id).ok_or(AllocatorError::InvalidMetadata)?;

        if !heap.is_active() {
            return heaps
                .free(heap_id, owner, ptr, pages)
                .map_err(AllocatorError::from);
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

        match heap.enqueue(heap_id, owner) {
            Ok(()) => Ok(()),
            // Close won: claim held, not queued — Draining push+flush (no stranded Queued).
            Err(HeapError::InvalidHeap) => {
                heaps
                    .enqueue(heap_id, owner)
                    .map_err(AllocatorError::from)?;
                heaps.flush(heap_id, pages).map_err(AllocatorError::from)
            }
            Err(error) => Err(AllocatorError::from(error)),
        }
    }

    /// Inner not yet installed: init then take the ordinary alloc path.
    #[inline(never)]
    fn alloc_uninit(&self, layout: Layout) -> *mut u8 {
        if self.init().is_none() {
            return null_mut();
        }
        // SAFETY: inner is installed; same contract as the public method.
        unsafe { self.alloc(layout) }
    }

    /// Page-cache miss / remote / extent after the TLS hit missed.
    #[inline(never)]
    fn dealloc_slow(inner: *mut AllocatorInner, ptr: NonNull<u8>) {
        let Some(inner) = NonNull::new(inner) else {
            Self::abort();
        };
        match THREAD_HEAP.with(|tls| tls.free_slow(inner, ptr)) {
            Ok(()) => {}
            Err(error) => Self::free_fail(inner, ptr, error),
        }
    }

    /// Current-run empty, unbound, or inner not yet installed.
    #[inline(never)]
    fn alloc_miss(&self, inner: *mut AllocatorInner, class: SizeClass, layout: Layout) -> *mut u8 {
        let Some(inner) = NonNull::new(inner) else {
            return self.alloc_uninit(layout);
        };
        if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc_miss(inner, class)) {
            return ptr.as_ptr();
        }
        Self::bind_alloc(inner, AllocKind::Run(class))
    }

    /// Bound-extent miss: TLS extent alloc, else bind.
    #[inline(never)]
    fn alloc_extent(inner: NonNull<AllocatorInner>, spec: LayoutSpec, init: ExtentInit) -> *mut u8 {
        // SAFETY: caller retains `inner` for this cold extent alloc.
        let pages = unsafe { inner.as_ref() }.pages();
        if let Some(ptr) = THREAD_HEAP.with(|tls| tls.alloc_extent(inner, spec, pages, init)) {
            return ptr.as_ptr();
        }
        Self::bind_alloc(inner, AllocKind::Extent(spec, init))
    }

    /// Cross-heap or domain-error after the TLS hit missed.
    #[cold]
    #[inline(never)]
    fn free_fail(inner: NonNull<AllocatorInner>, ptr: NonNull<u8>, error: ThreadFreeError) {
        match error {
            ThreadFreeError::Heap(_) => Self::abort(),
            ThreadFreeError::Remote => {
                // SAFETY: inner is retained by this Allocator.
                let pages = unsafe { inner.as_ref() }.pages();
                let Some(owner) = pages.get(ptr) else {
                    Self::abort();
                };
                if Self::free_remote(inner, owner, ptr).is_err() {
                    Self::abort();
                }
            }
        }
    }
}

/// Cold unbound alloc request — one bind/Active path for run and extent.
#[derive(Clone, Copy)]
enum AllocKind {
    Run(SizeClass),
    Extent(LayoutSpec, ExtentInit),
}

impl AllocatorInner {
    fn new(config: AllocatorConfig) -> Option<NonNull<Self>> {
        let storage = OsMemory::map(core::mem::size_of::<Self>())?;
        let inner = storage.base().cast::<Self>();

        // SAFETY: inner points to uniquely owned mmap storage aligned at least to a page boundary.
        // `storage` is moved into the value it backs; [`Drop`] unmaps it only after pages/heaps.
        unsafe {
            inner.as_ptr().write(Self {
                refs: AtomicU32::new(1),
                pages: ManuallyDrop::new(PageMap::new()),
                heaps: ManuallyDrop::new(Heaps::new(config)),
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
        // [`Drop`] drops pages → heaps → storage; nothing touches `inner` afterward.
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
    use crate::heap::thread::ThreadHeap;
    use crate::heap::{Extent, Heap, HeapMode, Run};
    use std::sync::mpsc;
    use std::thread;

    /// Lazily-initialized inner for an `Allocator` created in this test.
    fn allocator_inner_ptr(allocator: &Allocator) -> NonNull<AllocatorInner> {
        allocator.inner().or_else(|| allocator.init()).unwrap()
    }

    fn allocator_inner(allocator: &Allocator) -> &AllocatorInner {
        // SAFETY: inner is retained by `allocator` for the lifetime of this borrow.
        unsafe { allocator_inner_ptr(allocator).as_ref() }
    }

    fn bind_alloc_small(
        tls: &ThreadHeap,
        inner: NonNull<AllocatorInner>,
        layout: Layout,
    ) -> NonNull<u8> {
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        tls.alloc(inner.as_ptr(), class)
            .or_else(|| tls.alloc_miss(inner, class))
            .unwrap()
    }

    fn bind_alloc_extent(
        tls: &ThreadHeap,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        tls.alloc_extent(inner, spec, pages, init).unwrap()
    }

    fn run_of(pages: &PageMap, ptr: NonNull<u8>) -> NonNull<Run> {
        let PageOwner::Run(run) = pages.get(ptr).unwrap() else {
            panic!("expected a run-owned pointer");
        };
        run
    }

    fn extent_of(pages: &PageMap, ptr: NonNull<u8>) -> NonNull<Extent> {
        let PageOwner::Extent(extent) = pages.get(ptr).unwrap() else {
            panic!("expected an extent-owned pointer");
        };
        extent
    }

    fn alloc_live(
        tls: &ThreadHeap,
        inner: NonNull<AllocatorInner>,
        layout: Layout,
        n: u32,
    ) -> Vec<NonNull<u8>> {
        (0..n)
            .map(|_| bind_alloc_small(tls, inner, layout))
            .collect()
    }

    fn free_all(
        tls: &ThreadHeap,
        inner: NonNull<AllocatorInner>,
        pages: &PageMap,
        ptrs: &[NonNull<u8>],
    ) -> Result<(), ThreadFreeError> {
        let mut last = Ok(());
        for &ptr in ptrs {
            last = tls.free_run(inner, run_of(pages, ptr), ptr);
        }
        last
    }

    #[test]
    fn owner_free_publishes_immediately() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(inner, run, ptr), Ok(()));
            // SAFETY: owner free is locate + push; the block is on the run freelist.
            assert!(!unsafe { run.as_ref() }.is_live());
            assert_eq!(unsafe { run.as_ref() }.allocate(), Some(ptr));
            assert_eq!(tls.free_run(inner, run, ptr), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn current_run_switches_when_full() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let first = bind_alloc_small(tls, inner, layout);
            let run_a = run_of(pages, first);
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut ptrs = Vec::with_capacity(capacity + 1);
            ptrs.push(first);
            for _ in 1..capacity {
                ptrs.push(bind_alloc_small(tls, inner, layout));
            }
            // SAFETY: just filled this run.
            assert!(unsafe { run_a.as_ref() }.is_full());
            let extra = bind_alloc_small(tls, inner, layout);
            let run_b = run_of(pages, extra);
            assert_ne!(run_a, run_b);
            ptrs.push(extra);
            assert_eq!(free_all(tls, inner, pages, &ptrs), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn free_to_non_current_full_run_relinks_available() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut a_ptrs = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                a_ptrs.push(bind_alloc_small(tls, inner, layout));
            }
            let run_a = run_of(pages, a_ptrs[0]);
            // SAFETY: just filled this run.
            assert!(unsafe { run_a.as_ref() }.is_full());
            let b = bind_alloc_small(tls, inner, layout);
            let run_b = run_of(pages, b);
            assert_ne!(run_a, run_b);
            assert_eq!(tls.free_run(inner, run_a, a_ptrs[0]), Ok(()));
            let mut b_ptrs = vec![b];
            for _ in 1..capacity {
                b_ptrs.push(bind_alloc_small(tls, inner, layout));
            }
            // SAFETY: B is now full; next alloc must take A from available.
            assert!(unsafe { run_b.as_ref() }.is_full());
            let reused = bind_alloc_small(tls, inner, layout);
            assert_eq!(reused, a_ptrs[0]);
            assert_eq!(run_of(pages, reused), run_a);
            assert_eq!(tls.free_run(inner, run_a, reused), Ok(()));
            assert_eq!(free_all(tls, inner, pages, &a_ptrs[1..]), Ok(()));
            assert_eq!(free_all(tls, inner, pages, &b_ptrs), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn page_cache_stores_only_own_heap_runs() {
        let owner = Allocator::new();
        let other = Allocator::new();
        let inner = allocator_inner_ptr(&owner);
        let foreign = allocator_inner_ptr(&other);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let ptr = bind_alloc_small(tls, inner, layout);
            assert!(tls.cached_run(inner.as_ptr(), ptr).is_none());
            assert_eq!(tls.free_slow(inner, ptr), Ok(()));
            let again = bind_alloc_small(tls, inner, layout);
            assert_eq!(again, ptr);
            let Some(run) = tls.cached_run(inner.as_ptr(), again) else {
                panic!("page cache should hold the own-heap run after free_slow");
            };
            assert_eq!(tls.free_run(inner, run, again), Ok(()));
            let _ = tls.bind(foreign).unwrap();
            assert!(tls.cached_run(inner.as_ptr(), ptr).is_none());
            let _ = tls.bind(inner).unwrap();
            tls.unbind();
        });
    }

    #[test]
    fn unbind_with_current_runs_leaves_exact_live() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, ptr, run) = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            // SAFETY: user-held; unbind must not take / change live.
            assert!(unsafe { run.as_ref() }.is_live());
            tls.unbind();
            (id, ptr, run)
        });
        let inner_ref = allocator_inner(&allocator);
        assert_eq!(
            inner_ref.heaps.get(id).map(Heap::mode),
            Some(HeapMode::Draining)
        );
        // SAFETY: run stays arena-resident through Draining.
        assert!(unsafe { run.as_ref() }.is_live());
        assert_eq!(
            inner_ref
                .heaps
                .free(id, PageOwner::Run(run), ptr, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.heaps.get(id).is_none());
    }

    #[test]
    fn remote_claim_accept_publishes_once() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            // SAFETY: user-held block; claim is the remote admission path.
            assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
            assert!(!unsafe { run.as_ref() }.accept());
            assert_eq!(unsafe { run.as_ref() }.allocate(), Some(ptr));
            assert!(unsafe { run.as_ref() }.free(ptr).is_ok());
            tls.unbind();
        });
    }

    #[test]
    fn free_run_remote_on_foreign_allocator() {
        let owner = Allocator::new();
        let other = Allocator::new();
        let inner = allocator_inner_ptr(&owner);
        let foreign = allocator_inner_ptr(&other);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            assert_eq!(
                tls.free_run(foreign, run, ptr),
                Err(ThreadFreeError::Remote)
            );
            assert_eq!(tls.free_run(inner, run, ptr), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn allocator_extent_free_keeps_page_entry_while_cached() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_extent(tls, inner, pages, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            assert_eq!(tls.free_extent(inner, extent, ptr, pages), Ok(()));
            assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
            tls.unbind();
        });
    }

    #[test]
    fn allocator_extent_free_unpublishes_when_drop_policy() {
        let allocator = Allocator::with_config(
            AllocatorConfig::new().with_extent_policy(crate::config::ExtentPolicy::Drop),
        );
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_extent(tls, inner, pages, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            assert_eq!(tls.free_extent(inner, extent, ptr, pages), Ok(()));
            assert!(pages.get(ptr).is_none());
            tls.unbind();
        });
    }

    #[test]
    fn allocator_allocates_small_from_current_heap() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            // SAFETY: PageMap stores only live run pointers.
            assert_eq!(unsafe { run.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_run(inner, run, ptr), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn allocator_allocates_extent_from_current_heap() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_extent(tls, inner, pages, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            // SAFETY: PageMap stores only live extent pointers.
            assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_extent(inner, extent, ptr, pages), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let inner = allocator_inner_ptr(&allocator);
        THREAD_HEAP.with(|tls| {
            let _id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            // Heap stays Active (still bound). free_remote is the cross-thread path —
            // claim+enqueue twice must report DoubleFree on the second claim.
            assert_eq!(
                Allocator::free_remote(inner, PageOwner::Run(run), ptr),
                Ok(())
            );
            assert_eq!(
                Allocator::free_remote(inner, PageOwner::Run(run), ptr),
                Err(AllocatorError::DoubleFree)
            );
            tls.unbind();
        });
    }

    #[test]
    fn retained_remote_claim_completes_under_draining() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, run, ptr) = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            // SAFETY: block was just allocated from this run.
            assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
            tls.unbind();
            (id, run, ptr)
        });

        let inner_ref = allocator_inner(&allocator);
        assert_eq!(inner_ref.heaps.retire(id, inner_ref.pages()), Ok(()));
        assert_eq!(
            inner_ref.heaps.get(id).map(Heap::mode),
            Some(HeapMode::Draining)
        );
        assert_eq!(inner_ref.heaps.enqueue(id, PageOwner::Run(run)), Ok(()));
        assert_eq!(inner_ref.heaps.flush(id, inner_ref.pages()), Ok(()));
        assert!(inner_ref.heaps.get(id).is_none());
        let _ = ptr;
    }

    #[test]
    fn remote_frees_to_distinct_heaps_publish_independently_without_batching() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let inner = allocator_inner_ptr(&allocator);
        let inner_addr = inner.as_ptr() as usize;
        let (ready_a, wait_a) = mpsc::channel::<Vec<usize>>();
        let (ready_b, wait_b) = mpsc::channel::<Vec<usize>>();
        let (go_a, start_a) = mpsc::channel::<()>();
        let (go_b, start_b) = mpsc::channel::<()>();
        let (done_a, finished_a) = mpsc::channel::<bool>();
        let (done_b, finished_b) = mpsc::channel::<bool>();

        thread::scope(|scope| {
            scope.spawn(move || {
                // SAFETY: `inner_addr` is the test allocator's live inner for this scope.
                let inner = NonNull::new(inner_addr as *mut AllocatorInner).unwrap();
                let start_a = start_a;
                let ready_a = ready_a;
                let done_a = done_a;
                THREAD_HEAP.with(|tls| {
                    let _id = tls.bind(inner).unwrap();
                    // SAFETY: inner retained by allocator.
                    let pages = unsafe { inner.as_ref() }.pages();
                    let live = alloc_live(tls, inner, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_a
                        .send(live.iter().map(|p| p.as_ptr() as usize).collect())
                        .unwrap();
                    start_a.recv().unwrap();
                    assert_eq!(tls.flush(inner, pages), Ok(()));
                    // SAFETY: run from this heap's arena.
                    done_a.send(unsafe { run.as_ref() }.is_live()).unwrap();
                    tls.unbind();
                });
            });
            scope.spawn(move || {
                // SAFETY: `inner_addr` is the test allocator's live inner for this scope.
                let inner = NonNull::new(inner_addr as *mut AllocatorInner).unwrap();
                let start_b = start_b;
                let ready_b = ready_b;
                let done_b = done_b;
                THREAD_HEAP.with(|tls| {
                    let _id = tls.bind(inner).unwrap();
                    // SAFETY: inner retained by allocator.
                    let pages = unsafe { inner.as_ref() }.pages();
                    let live = alloc_live(tls, inner, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_b
                        .send(live.iter().map(|p| p.as_ptr() as usize).collect())
                        .unwrap();
                    start_b.recv().unwrap();
                    assert_eq!(tls.flush(inner, pages), Ok(()));
                    // SAFETY: run from this heap's arena.
                    done_b.send(unsafe { run.as_ref() }.is_live()).unwrap();
                    tls.unbind();
                });
            });

            let addrs_a = wait_a.recv().unwrap();
            let addrs_b = wait_b.recv().unwrap();
            // SAFETY: owners still bound; PageMap entries live.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr_a = NonNull::new(addrs_a[0] as *mut u8).unwrap();
            let ptr_b = NonNull::new(addrs_b[0] as *mut u8).unwrap();
            let run_a = run_of(pages, ptr_a);
            let run_b = run_of(pages, ptr_b);
            for addr in addrs_a {
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                assert_eq!(
                    Allocator::free_remote(inner, PageOwner::Run(run_a), ptr),
                    Ok(())
                );
            }
            for addr in addrs_b {
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                assert_eq!(
                    Allocator::free_remote(inner, PageOwner::Run(run_b), ptr),
                    Ok(())
                );
            }
            go_a.send(()).unwrap();
            go_b.send(()).unwrap();
            assert!(!finished_a.recv().unwrap());
            assert!(!finished_b.recv().unwrap());
        });
    }

    #[test]
    fn concurrent_active_leases_exact_once() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 16;

        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        // SAFETY: inner retained by allocator.
        let inner_ref = unsafe { inner.as_ref() };
        let pages = inner_ref.pages();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();

        let (id, run_addr, addrs) = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            let mut addrs = Vec::with_capacity(THREADS * PER_THREAD);
            for _ in 0..THREADS * PER_THREAD {
                addrs.push(
                    tls.alloc(inner.as_ptr(), class)
                        .or_else(|| tls.alloc_miss(inner, class))
                        .unwrap()
                        .as_ptr() as usize,
                );
            }
            let run = run_of(pages, NonNull::new(addrs[0] as *mut u8).unwrap());
            (id, run.as_ptr() as usize, addrs)
        });

        let heap = inner_ref.heaps.get(id).unwrap();
        let addrs = &addrs[..];
        let heap_addr = core::ptr::from_ref(heap) as usize;

        thread::scope(|scope| {
            for t in 0..THREADS {
                scope.spawn(move || {
                    // SAFETY: heap stays Active and published for this test scope.
                    let heap = unsafe { &*(heap_addr as *const Heap) };
                    let run = NonNull::new(run_addr as *mut Run).unwrap();
                    let start = t * PER_THREAD;
                    for &addr in &addrs[start..start + PER_THREAD] {
                        let ptr = NonNull::new(addr as *mut u8).unwrap();
                        // SAFETY: addr is a block owned by `run`, allocated above.
                        unsafe { run.as_ref() }.claim(ptr).unwrap();
                        assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
                    }
                });
            }
        });

        assert_eq!(heap.leases(), 0);
        THREAD_HEAP.with(|tls| {
            assert_eq!(tls.flush(inner, pages), Ok(()));
            let run = NonNull::new(run_addr as *mut Run).unwrap();
            // SAFETY: same run pointer from this heap's live arena.
            assert!(!unsafe { run.as_ref() }.is_live());
            tls.unbind();
        });
    }

    #[test]
    fn reclaim_rejects_nonempty_run_inbox() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let id = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let live = alloc_live(tls, inner, layout, 8);
            let run = run_of(pages, live[0]);
            for ptr in live {
                // SAFETY: block was just allocated from this run.
                unsafe { run.as_ref() }.claim(ptr).unwrap();
            }
            let heap = unsafe { inner.as_ref() }.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        });

        let inner_ref = allocator_inner(&allocator);
        assert_eq!(inner_ref.heaps.reclaim(id), Ok(()));
        assert!(inner_ref.heaps.get(id).is_some());
        assert_eq!(inner_ref.heaps.flush(id, inner_ref.pages()), Ok(()));
        assert!(inner_ref.heaps.get(id).is_none());
        THREAD_HEAP.with(ThreadHeap::unbind);
    }

    #[test]
    fn reclaim_rejects_nonempty_extent_inbox() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let id = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_extent(tls, inner, pages, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            // SAFETY: extent is live and Allocated.
            unsafe { extent.as_ref() }.claim(ptr).unwrap();
            let heap = unsafe { inner.as_ref() }.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Extent(extent)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        });

        let inner_ref = allocator_inner(&allocator);
        assert_eq!(inner_ref.heaps.reclaim(id), Ok(()));
        assert!(inner_ref.heaps.get(id).is_some());
        assert_eq!(inner_ref.heaps.flush(id, inner_ref.pages()), Ok(()));
        assert!(inner_ref.heaps.get(id).is_none());
        THREAD_HEAP.with(ThreadHeap::unbind);
    }

    #[test]
    fn allocator_tracks_live_run_allocations_through_draining_reclaim() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, first_run, second, second_run) = THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let first = bind_alloc_small(tls, inner, layout);
            let second = bind_alloc_small(tls, inner, layout);
            let first_run = run_of(pages, first);
            let second_run = run_of(pages, second);
            tls.unbind();
            (id, first, first_run, second, second_run)
        });

        let inner_ref = allocator_inner(&allocator);
        // unbind already retired; heap should be Draining with live blocks.
        assert_eq!(
            inner_ref.heaps.get(id).map(Heap::mode),
            Some(HeapMode::Draining)
        );
        assert_eq!(
            inner_ref
                .heaps
                .free(id, PageOwner::Run(first_run), first, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.heaps.get(id).is_some());
        assert_eq!(
            inner_ref
                .heaps
                .free(id, PageOwner::Run(second_run), second, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.heaps.get(id).is_none());
    }

    #[test]
    fn allocator_reuses_released_heap_after_draining_free() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr, run) = THREAD_HEAP.with(|tls| {
            let heap = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            tls.unbind();
            (heap, ptr, run)
        });

        let inner_ref = allocator_inner(&allocator);
        assert_eq!(
            inner_ref
                .heaps
                .free(heap, PageOwner::Run(run), ptr, inner_ref.pages()),
            Ok(())
        );
        assert!(inner_ref.pages().get(ptr).is_some());
        THREAD_HEAP.with(|tls| {
            let reused = tls.bind(inner).unwrap();
            assert_eq!(reused.index(), heap.index());
            tls.unbind();
        });
    }

    #[test]
    fn allocator_release_retains_empty_heap_run_page_entry_for_reuse() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr) = THREAD_HEAP.with(|tls| {
            let heap = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_small(tls, inner, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(inner, run, ptr), Ok(()));
            assert!(pages.get(ptr).is_some());
            tls.unbind();
            (heap, ptr)
        });

        let pages = allocator_inner(&allocator).pages();
        assert!(pages.get(ptr).is_some());

        THREAD_HEAP.with(|tls| {
            let reused = tls.bind(inner).unwrap();
            assert_eq!(reused.index(), heap.index());
            assert_ne!(reused.generation(), heap.generation());
            let pages = unsafe { inner.as_ref() }.pages();
            let reused_ptr = bind_alloc_small(tls, inner, layout);
            assert!(pages.get(ptr).is_some());
            assert!(pages.get(reused_ptr).is_some());
            let reused_run = run_of(pages, reused_ptr);
            assert_eq!(tls.free_run(inner, reused_run, reused_ptr), Ok(()));
            tls.unbind();
        });
    }

    #[test]
    fn allocator_zeroed_large_allocation_uses_current_heap() {
        let allocator = Allocator::new();
        let inner = allocator_inner_ptr(&allocator);
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        THREAD_HEAP.with(|tls| {
            let id = tls.bind(inner).unwrap();
            // SAFETY: inner retained by allocator.
            let pages = unsafe { inner.as_ref() }.pages();
            let ptr = bind_alloc_extent(tls, inner, pages, layout, ExtentInit::Zeroed);
            // SAFETY: ptr was just allocated zeroed for layout.
            assert!(
                unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) }
                    .iter()
                    .all(|&byte| byte == 0)
            );
            let extent = extent_of(pages, ptr);
            // SAFETY: PageMap stores only live extent pointers.
            assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_extent(inner, extent, ptr, pages), Ok(()));
            tls.unbind();
        });
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

        let inner = allocator_inner(&allocator);
        let id = unsafe {
            run_of(inner.pages(), NonNull::new(ptr).unwrap())
                .as_ref()
                .heap_id()
        };

        // SAFETY: ptr was returned by alloc(small) above and is not yet freed.
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let extent = extent_of(inner.pages(), NonNull::new(grown).unwrap());

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        // SAFETY: grown was returned by realloc above for large.
        unsafe { allocator.dealloc(grown, large) };
    }
}
