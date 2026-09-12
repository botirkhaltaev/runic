use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    config::AllocatorConfig,
    heap::extent::ExtentError,
    heap::{
        AllocatorCtx, ExtentInit, HeapError, Heaps, RunError, THREAD_HEAP, ThreadFreeError,
        ThreadHeap,
    },
    layout::LayoutSpec,
    memory::{OsMemory, PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

pub struct Allocator {
    config: AllocatorConfig,
}

/// mmap payload for [`AllocatorCtx`]. Not returned to callers.
struct Process {
    pages: PageMap,
    heaps: Heaps,
}

static PROCESS: AtomicPtr<Process> = AtomicPtr::new(core::ptr::null_mut());

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

    /// First `init` in the process wins; later configs are ignored.
    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self { config }
    }

    /// Installed pages and heaps, or `None` before first `init`.
    #[inline]
    pub(crate) fn ctx() -> Option<AllocatorCtx<'static>> {
        let process = NonNull::new(PROCESS.load(Ordering::Acquire))?;
        // SAFETY: installed payload lives for the process lifetime.
        let process = unsafe { process.as_ref() };
        Some(AllocatorCtx {
            pages: &process.pages,
            heaps: &process.heaps,
        })
    }

    /// Allocates memory for `layout` using this allocator's state.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw, uninitialized memory. The caller must use it
    /// only according to `layout`, avoid out-of-bounds access, and eventually
    /// pass the same pointer and a compatible layout back to this allocator.
    #[must_use]
    #[inline]
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        if let Some(class) = SizeClasses::class_for(spec) {
            if let Some(ptr) = THREAD_HEAP.alloc(class) {
                return ptr.as_ptr();
            }
            return self.alloc_miss(class, layout);
        }
        if let Some(ctx) = Self::ctx() {
            return Self::alloc_extent(&ctx, spec, ExtentInit::Uninit);
        }
        self.alloc_uninit(layout)
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer previously returned by this allocator for
    /// `layout`. Null is forbidden (`GlobalAlloc` contract) and is fail-closed
    /// (`PageMap` miss → abort), not accepted. Passing an unknown pointer, an
    /// interior pointer, or an incompatible layout violates the allocator
    /// contract and may abort.
    #[inline]
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            Self::abort();
        };
        let spec = LayoutSpec::from_layout(layout);
        if let Some(class) = SizeClasses::class_for(spec)
            && THREAD_HEAP.free(ptr, class).is_some()
        {
            return;
        }
        Self::dealloc_slow(ptr, spec);
    }

    /// Changes the size of an allocation using allocate-copy-free semantics.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `old`. If a non-null pointer is supplied, no other live reference may
    /// be used to access the old allocation after successful reallocation.
    #[must_use]
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

        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        // SAFETY: `ptr` is non-null after the early return above.
        let old_ptr = unsafe { NonNull::new_unchecked(ptr) };
        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return null_mut();
        };
        let new_spec = LayoutSpec::from_layout(new_layout);
        let old_spec = LayoutSpec::from_layout(old);

        let resized = match ThreadHeap::lookup(ctx.pages, old_ptr, old_spec) {
            Some(PageOwner::Run(run)) => {
                // SAFETY: lookup returns a live arena run; resize still `locate`s.
                unsafe { run.as_ref() }
                    .resize_in_place(old_ptr, new_spec)
                    .map_err(AllocatorError::from)
            }
            Some(PageOwner::Extent(mut extent)) => {
                // SAFETY: lookup returns a live arena extent.
                unsafe { extent.as_mut() }
                    .resize_in_place(old_ptr, new_spec)
                    .map_err(AllocatorError::from)
            }
            None => Self::abort(),
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
    #[must_use]
    #[inline]
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);
        let Some(class) = SizeClasses::class_for(spec) else {
            if let Some(ctx) = Self::ctx() {
                return Self::alloc_extent(&ctx, spec, ExtentInit::Zeroed);
            }
            if self.init().is_none() {
                return null_mut();
            }
            // SAFETY: process is installed; same contract as the public method.
            return unsafe { self.alloc_zeroed(layout) };
        };

        let ptr = if let Some(ptr) = THREAD_HEAP.alloc(class) {
            ptr.as_ptr()
        } else {
            self.alloc_miss(class, layout)
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

    #[cold]
    #[inline(never)]
    fn init(&self) -> Option<AllocatorCtx<'static>> {
        let mapping = OsMemory::map(core::mem::size_of::<Process>())?;
        let process = mapping.base().cast::<Process>();
        // SAFETY: `process` is uniquely owned page-aligned mmap. Fields are
        // written before the CAS publishes the pointer.
        unsafe {
            process.as_ptr().write(Process {
                pages: PageMap::new(),
                heaps: Heaps::new(self.config),
            });
        }
        if PROCESS
            .compare_exchange(
                core::ptr::null_mut(),
                process.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            core::mem::forget(mapping);
        } else {
            // SAFETY: this thread uniquely owns the uninstalled process;
            // `PageMap` / `Heaps` Drop must run while the mapping is live.
            unsafe { process.as_ptr().drop_in_place() };
            drop(mapping);
        }
        Self::ctx()
    }

    /// Not owner-local on TLS: bind Active heap, then flush-then-alloc (run or extent).
    #[cold]
    #[inline(never)]
    fn bind_alloc(ctx: &AllocatorCtx<'_>, request: AllocKind) -> *mut u8 {
        if THREAD_HEAP.bind(ctx).is_none() {
            return null_mut();
        }
        if THREAD_HEAP.flush(ctx).is_err() {
            return null_mut();
        }
        match request {
            AllocKind::Run(class) => THREAD_HEAP.alloc_miss(class, ctx),
            AllocKind::Extent(spec, init) => THREAD_HEAP.alloc_extent(spec, init, ctx),
        }
        .map_or(null_mut(), NonNull::as_ptr)
    }

    /// Cross-heap free: adopt a Draining heap, else Active claim → enqueue,
    /// else `Heaps::free` under Draining.
    ///
    /// Coalescing is by owner inbox. `Remote` callers only — heap-domain errors abort
    /// in `dealloc` before this runs.
    #[cold]
    #[inline(never)]
    fn free_remote(
        ctx: &AllocatorCtx<'_>,
        owner: PageOwner,
        ptr: NonNull<u8>,
    ) -> Result<(), AllocatorError> {
        let heap_id = owner.heap_id();
        let heap = match owner {
            // SAFETY: PageMap / header_of store only live arena run pointers.
            PageOwner::Run(run) => unsafe { run.as_ref() }.heap(),
            PageOwner::Extent(_) => None,
        }
        .or_else(|| ctx.heaps.get(heap_id))
        .ok_or(AllocatorError::InvalidMetadata)?;

        if !heap.is_active() {
            if THREAD_HEAP.adopt(heap, heap_id, ctx) {
                return THREAD_HEAP
                    .free_owner(owner, ptr, ctx)
                    .map_err(|error| match error {
                        ThreadFreeError::Heap(error) => AllocatorError::from(error),
                        ThreadFreeError::Remote(_) => AllocatorError::InvalidMetadata,
                    });
            }
            match ctx.heaps.free(heap_id, owner, ptr, ctx) {
                Ok(()) => return Ok(()),
                // Another thread won `adopt`; heap is now Active.
                Err(HeapError::InvalidHeap) => {}
                Err(error) => return Err(AllocatorError::from(error)),
            }
            if !heap.is_active() {
                return Err(AllocatorError::InvalidMetadata);
            }
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
                ctx.heaps
                    .enqueue(heap_id, owner)
                    .map_err(AllocatorError::from)?;
                ctx.heaps.flush(heap_id, ctx).map_err(AllocatorError::from)
            }
            Err(error) => Err(AllocatorError::from(error)),
        }
    }

    /// Process state not yet installed: init then take the ordinary alloc path.
    #[inline(never)]
    fn alloc_uninit(&self, layout: Layout) -> *mut u8 {
        if self.init().is_none() {
            return null_mut();
        }
        // SAFETY: process is installed; same contract as the public method.
        unsafe { self.alloc(layout) }
    }

    /// Current-run miss, large, or unbound: `lookup` then typed free.
    #[inline(never)]
    fn dealloc_slow(ptr: NonNull<u8>, spec: LayoutSpec) {
        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        match THREAD_HEAP.free_slow(ptr, spec, &ctx) {
            Ok(()) => {}
            Err(error) => Self::free_fail(&ctx, ptr, error),
        }
    }

    /// Current-run empty, unbound, or state not yet installed.
    #[inline(never)]
    fn alloc_miss(&self, class: SizeClass, layout: Layout) -> *mut u8 {
        let Some(ctx) = Self::ctx() else {
            return self.alloc_uninit(layout);
        };
        if let Some(ptr) = THREAD_HEAP.alloc_miss(class, &ctx) {
            return ptr.as_ptr();
        }
        Self::bind_alloc(&ctx, AllocKind::Run(class))
    }

    /// Bound-extent miss: TLS extent alloc, else bind.
    #[inline(never)]
    fn alloc_extent(ctx: &AllocatorCtx<'_>, spec: LayoutSpec, init: ExtentInit) -> *mut u8 {
        if let Some(ptr) = THREAD_HEAP.alloc_extent(spec, init, ctx) {
            return ptr.as_ptr();
        }
        Self::bind_alloc(ctx, AllocKind::Extent(spec, init))
    }

    /// Cross-heap or domain-error after the TLS hit missed.
    #[cold]
    #[inline(never)]
    fn free_fail(ctx: &AllocatorCtx<'_>, ptr: NonNull<u8>, error: ThreadFreeError) {
        match error {
            ThreadFreeError::Heap(_) => Self::abort(),
            ThreadFreeError::Remote(owner) => {
                if Self::free_remote(ctx, owner, ptr).is_err() {
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

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl From<RunError> for AllocatorError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidPointer | RunError::OutOfRange => Self::InvalidRunPointer,
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
    use std::sync::{Barrier, mpsc};
    use std::thread;

    fn install(allocator: &Allocator) -> AllocatorCtx<'static> {
        allocator
            .init()
            .or_else(Allocator::ctx)
            .expect("allocator ctx")
    }

    fn unbind(tls: &ThreadHeap) {
        let ctx = Allocator::ctx().expect("allocator ctx");
        tls.unbind(&ctx);
    }

    fn bind_alloc_small(tls: &ThreadHeap, ctx: &AllocatorCtx<'_>, layout: Layout) -> NonNull<u8> {
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        tls.alloc(class)
            .or_else(|| tls.alloc_miss(class, ctx))
            .unwrap()
    }

    fn bind_alloc_extent(
        tls: &ThreadHeap,
        ctx: &AllocatorCtx<'_>,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        tls.alloc_extent(spec, init, ctx).unwrap()
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
        ctx: &AllocatorCtx<'_>,
        layout: Layout,
        n: u32,
    ) -> Vec<NonNull<u8>> {
        (0..n).map(|_| bind_alloc_small(tls, ctx, layout)).collect()
    }

    fn free_all(
        tls: &ThreadHeap,
        pages: &PageMap,
        ptrs: &[NonNull<u8>],
    ) -> Result<(), ThreadFreeError> {
        let mut last = Ok(());
        for &ptr in ptrs {
            last = tls.free_run(run_of(pages, ptr), ptr);
        }
        last
    }

    #[test]
    fn current_run_free_hits_without_lookup() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            assert_eq!(tls.free(ptr, class), Some(()));
            assert_eq!(tls.alloc(class), Some(ptr));
            assert_eq!(tls.free(ptr, class), Some(()));
            unbind(tls);
        };
    }

    #[test]
    fn owner_free_publishes_immediately() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            // SAFETY: owner free is locate + push; the block is on the run freelist.
            assert!(!unsafe { run.as_ref() }.is_live());
            assert_eq!(unsafe { run.as_ref() }.allocate(), Some(ptr));
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn current_run_switches_when_full() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let first = bind_alloc_small(tls, &ctx, layout);
            let run_a = run_of(pages, first);
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut ptrs = Vec::with_capacity(capacity + 1);
            ptrs.push(first);
            for _ in 1..capacity {
                ptrs.push(bind_alloc_small(tls, &ctx, layout));
            }
            // SAFETY: just filled this run.
            assert!(unsafe { run_a.as_ref() }.is_full());
            let extra = bind_alloc_small(tls, &ctx, layout);
            let run_b = run_of(pages, extra);
            assert_ne!(run_a, run_b);
            ptrs.push(extra);
            assert_eq!(free_all(tls, pages, &ptrs), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn free_to_non_current_full_run_relinks_available() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut a_ptrs = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                a_ptrs.push(bind_alloc_small(tls, &ctx, layout));
            }
            let run_a = run_of(pages, a_ptrs[0]);
            // SAFETY: just filled this run.
            assert!(unsafe { run_a.as_ref() }.is_full());
            let b = bind_alloc_small(tls, &ctx, layout);
            let run_b = run_of(pages, b);
            assert_ne!(run_a, run_b);
            assert_eq!(tls.free_run(run_a, a_ptrs[0]), Ok(()));
            let mut b_ptrs = vec![b];
            for _ in 1..capacity {
                b_ptrs.push(bind_alloc_small(tls, &ctx, layout));
            }
            // SAFETY: B is now full; next alloc must take A from available.
            assert!(unsafe { run_b.as_ref() }.is_full());
            let reused = bind_alloc_small(tls, &ctx, layout);
            assert_eq!(reused, a_ptrs[0]);
            assert_eq!(run_of(pages, reused), run_a);
            assert_eq!(tls.free_run(run_a, reused), Ok(()));
            assert_eq!(free_all(tls, pages, &a_ptrs[1..]), Ok(()));
            assert_eq!(free_all(tls, pages, &b_ptrs), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn header_of_finds_in_page_run_after_free() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let spec = LayoutSpec::from_layout(layout);
            let Some(PageOwner::Run(run)) = ThreadHeap::lookup(ctx.pages, ptr, spec) else {
                panic!("alloc should publish a run");
            };
            assert_eq!(Run::header_of(ptr), Some(run));
            assert_eq!(tls.free_slow(ptr, spec, &ctx), Ok(()));
            let again = bind_alloc_small(tls, &ctx, layout);
            assert_eq!(again, ptr);
            assert_eq!(Run::header_of(again), Some(run));
            assert_eq!(tls.free_run(run, again), Ok(()));
            unbind(tls);
            assert_eq!(
                ThreadHeap::lookup(ctx.pages, ptr, spec),
                Some(PageOwner::Run(run))
            );
        };
    }

    #[test]
    fn unbind_with_current_runs_leaves_exact_live() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, ptr, run) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // SAFETY: user-held; unbind must not take / change live.
            assert!(unsafe { run.as_ref() }.is_live());
            unbind(tls);
            (id, ptr, run)
        };
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        // SAFETY: run stays arena-resident through Draining.
        assert!(unsafe { run.as_ref() }.is_live());
        assert_eq!(ctx.heaps.free(id, PageOwner::Run(run), ptr, &ctx), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn remote_claim_accept_publishes_once() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // SAFETY: user-held block; claim is the remote admission path.
            assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
            assert!(!unsafe { run.as_ref() }.accept());
            assert_eq!(unsafe { run.as_ref() }.allocate(), Some(ptr));
            assert!(unsafe { run.as_ref() }.free(ptr).is_ok());
            unbind(tls);
        };
    }

    #[test]
    fn allocator_extent_free_keeps_page_entry_while_cached() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
            unbind(tls);
        };
    }

    #[test]
    fn allocator_allocates_small_from_current_heap() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // SAFETY: PageMap stores only live run pointers.
            assert_eq!(unsafe { run.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn allocator_allocates_extent_from_current_heap() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            // SAFETY: PageMap stores only live extent pointers.
            assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // Heap stays Active (still bound). free_remote is the cross-thread path —
            // claim+enqueue twice must report DoubleFree on the second claim.
            assert_eq!(
                Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                Ok(())
            );
            assert_eq!(
                Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                Err(AllocatorError::DoubleFree)
            );
            unbind(tls);
        };
    }

    #[test]
    fn retained_remote_claim_completes_under_draining() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, run, ptr) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // SAFETY: block was just allocated from this run.
            assert_eq!(unsafe { run.as_ref() }.claim(ptr), Ok(()));
            unbind(tls);
            (id, run, ptr)
        };

        assert_eq!(ctx.heaps.retire(id, &ctx), Ok(()));
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(ctx.heaps.enqueue(id, PageOwner::Run(run)), Ok(()));
        assert_eq!(ctx.heaps.flush(id, &ctx), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
        let _ = ptr;
    }

    #[test]
    fn remote_frees_to_distinct_heaps_publish_independently_without_batching() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let (ready_a, wait_a) = mpsc::channel::<Vec<usize>>();
        let (ready_b, wait_b) = mpsc::channel::<Vec<usize>>();
        let (go_a, start_a) = mpsc::channel::<()>();
        let (go_b, start_b) = mpsc::channel::<()>();
        let (done_a, finished_a) = mpsc::channel::<bool>();
        let (done_b, finished_b) = mpsc::channel::<bool>();

        thread::scope(|scope| {
            scope.spawn(move || {
                let start_a = start_a;
                let ready_a = ready_a;
                let done_a = done_a;
                {
                    let tls = &THREAD_HEAP;
                    let _id = tls.bind(&ctx).unwrap();
                    let live = alloc_live(tls, &ctx, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_a
                        .send(live.iter().map(|p| p.as_ptr() as usize).collect())
                        .unwrap();
                    start_a.recv().unwrap();
                    assert_eq!(tls.flush(&ctx), Ok(()));
                    // SAFETY: run from this heap's arena.
                    done_a.send(unsafe { run.as_ref() }.is_live()).unwrap();
                    unbind(tls);
                };
            });
            scope.spawn(move || {
                let start_b = start_b;
                let ready_b = ready_b;
                let done_b = done_b;
                {
                    let tls = &THREAD_HEAP;
                    let _id = tls.bind(&ctx).unwrap();
                    let live = alloc_live(tls, &ctx, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_b
                        .send(live.iter().map(|p| p.as_ptr() as usize).collect())
                        .unwrap();
                    start_b.recv().unwrap();
                    assert_eq!(tls.flush(&ctx), Ok(()));
                    // SAFETY: run from this heap's arena.
                    done_b.send(unsafe { run.as_ref() }.is_live()).unwrap();
                    unbind(tls);
                };
            });

            let addrs_a = wait_a.recv().unwrap();
            let addrs_b = wait_b.recv().unwrap();
            // SAFETY: owners still bound; PageMap entries live.
            let ptr_a = NonNull::new(addrs_a[0] as *mut u8).unwrap();
            let ptr_b = NonNull::new(addrs_b[0] as *mut u8).unwrap();
            let run_a = run_of(pages, ptr_a);
            let run_b = run_of(pages, ptr_b);
            for addr in addrs_a {
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                assert_eq!(
                    Allocator::free_remote(&ctx, PageOwner::Run(run_a), ptr),
                    Ok(())
                );
            }
            for addr in addrs_b {
                let ptr = NonNull::new(addr as *mut u8).unwrap();
                assert_eq!(
                    Allocator::free_remote(&ctx, PageOwner::Run(run_b), ptr),
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
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();

        let (id, run_addr, addrs) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let mut addrs = Vec::with_capacity(THREADS * PER_THREAD);
            for _ in 0..THREADS * PER_THREAD {
                addrs.push(
                    tls.alloc(class)
                        .or_else(|| tls.alloc_miss(class, &ctx))
                        .unwrap()
                        .as_ptr() as usize,
                );
            }
            let run = run_of(pages, NonNull::new(addrs[0] as *mut u8).unwrap());
            (id, run.as_ptr() as usize, addrs)
        };

        let heap = ctx.heaps.get(id).unwrap();
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
        {
            let tls = &THREAD_HEAP;
            assert_eq!(tls.flush(&ctx), Ok(()));
            let run = NonNull::new(run_addr as *mut Run).unwrap();
            // SAFETY: same run pointer from this heap's live arena.
            assert!(!unsafe { run.as_ref() }.is_live());
            unbind(tls);
        };
    }

    #[test]
    fn reclaim_rejects_nonempty_run_inbox() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let id = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let live = alloc_live(tls, &ctx, layout, 8);
            let run = run_of(pages, live[0]);
            for ptr in live {
                // SAFETY: block was just allocated from this run.
                unsafe { run.as_ref() }.claim(ptr).unwrap();
            }
            let heap = ctx.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        };

        assert_eq!(ctx.heaps.reclaim(id), Ok(()));
        assert!(ctx.heaps.get(id).is_some());
        assert_eq!(ctx.heaps.flush(id, &ctx), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
        unbind(&THREAD_HEAP);
    }

    #[test]
    fn reclaim_rejects_nonempty_extent_inbox() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let id = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            // SAFETY: extent is live and Allocated.
            unsafe { extent.as_ref() }.claim(ptr).unwrap();
            let heap = ctx.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Extent(extent)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        };

        assert_eq!(ctx.heaps.reclaim(id), Ok(()));
        assert!(ctx.heaps.get(id).is_some());
        assert_eq!(ctx.heaps.flush(id, &ctx), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
        unbind(&THREAD_HEAP);
    }

    #[test]
    fn allocator_tracks_live_run_allocations_through_draining_reclaim() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, first_run, second, second_run) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let first = bind_alloc_small(tls, &ctx, layout);
            let second = bind_alloc_small(tls, &ctx, layout);
            let first_run = run_of(pages, first);
            let second_run = run_of(pages, second);
            unbind(tls);
            (id, first, first_run, second, second_run)
        };

        // unbind already retired; heap should be Draining with live blocks.
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(
            ctx.heaps.free(id, PageOwner::Run(first_run), first, &ctx),
            Ok(())
        );
        assert!(ctx.heaps.get(id).is_some());
        assert_eq!(
            ctx.heaps.free(id, PageOwner::Run(second_run), second, &ctx),
            Ok(())
        );
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn allocator_reuses_released_heap_after_draining_free() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr, run) = {
            let tls = &THREAD_HEAP;
            let heap = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            unbind(tls);
            (heap, ptr, run)
        };

        assert_eq!(ctx.heaps.free(heap, PageOwner::Run(run), ptr, &ctx), Ok(()));
        assert!(pages.get(ptr).is_some());
        {
            let tls = &THREAD_HEAP;
            let reused = tls.bind(&ctx).unwrap();
            if reused.index() == heap.index() {
                assert_ne!(reused.generation(), heap.generation());
            }
            unbind(tls);
        };
    }

    #[test]
    fn allocator_release_retains_empty_heap_run_page_entry_for_reuse() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr) = {
            let tls = &THREAD_HEAP;
            let heap = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            assert!(pages.get(ptr).is_some());
            unbind(tls);
            (heap, ptr)
        };

        assert!(pages.get(ptr).is_some());

        {
            let tls = &THREAD_HEAP;
            let reused = tls.bind(&ctx).unwrap();
            if reused.index() == heap.index() {
                assert_ne!(reused.generation(), heap.generation());
            }
            let reused_ptr = bind_alloc_small(tls, &ctx, layout);
            assert!(pages.get(ptr).is_some());
            assert!(pages.get(reused_ptr).is_some());
            let reused_run = run_of(pages, reused_ptr);
            assert_eq!(tls.free_run(reused_run, reused_ptr), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn allocator_zeroed_large_allocation_uses_current_heap() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let ptr = bind_alloc_extent(tls, &ctx, layout, ExtentInit::Zeroed);
            // SAFETY: ptr was just allocated zeroed for layout.
            assert!(
                unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) }
                    .iter()
                    .all(|&byte| byte == 0)
            );
            let extent = extent_of(pages, ptr);
            // SAFETY: PageMap stores only live extent pointers.
            assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            unbind(tls);
        };
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

        let pages = Allocator::ctx().expect("allocator ctx").pages;
        let id = unsafe { run_of(pages, NonNull::new(ptr).unwrap()).as_ref().heap_id() };

        // SAFETY: ptr was returned by alloc(small) above and is not yet freed.
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let extent = extent_of(pages, NonNull::new(grown).unwrap());

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(unsafe { extent.as_ref() }.heap_id(), id);

        // SAFETY: grown was returned by realloc above for large.
        unsafe { allocator.dealloc(grown, large) };
    }

    #[test]
    fn dealloc_mixed_class_reverse_drop_uses_current() {
        let allocator = Allocator::new();
        let eight = Layout::from_size_align(8, 8).unwrap();
        let sixty_four = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: layouts are valid.
        let a = unsafe { allocator.alloc(eight) };
        let b = unsafe { allocator.alloc(sixty_four) };
        assert!(!a.is_null() && !b.is_null());
        // SAFETY: matching alloc/dealloc pairs.
        unsafe { allocator.dealloc(b, sixty_four) };
        unsafe { allocator.dealloc(a, eight) };
    }

    #[test]
    fn dealloc_non_current_same_class_falls_back_to_pagemap() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (first0, rest, extra, run_a, run_b) = {
            let tls = &THREAD_HEAP;
            let _id = tls.bind(&ctx).unwrap();
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut first = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                first.push(bind_alloc_small(tls, &ctx, layout));
            }
            let extra = bind_alloc_small(tls, &ctx, layout);
            let run_a = run_of(pages, first[0]);
            let run_b = run_of(pages, extra);
            assert_ne!(run_a, run_b);
            let first0 = first[0];
            let rest = first[1..].to_vec();
            (first0, rest, extra, run_a, run_b)
        };
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        assert_eq!(THREAD_HEAP.free(first0, class), None);
        // SAFETY: first0 is a live block on the non-current full run.
        unsafe { allocator.dealloc(first0.as_ptr(), layout) };
        {
            let tls = &THREAD_HEAP;
            assert_eq!(
                ThreadHeap::lookup(pages, first0, LayoutSpec::from_layout(layout)),
                Some(PageOwner::Run(run_a))
            );
            assert_eq!(tls.free_run(run_b, extra), Ok(()));
            assert_eq!(free_all(tls, pages, &rest), Ok(()));
            unbind(tls);
        };
    }

    #[test]
    fn realloc_in_class_stays_on_current_run() {
        let allocator = Allocator::new();
        let old = Layout::from_size_align(16, 8).unwrap();
        let new = Layout::from_size_align(24, 8).unwrap();
        // SAFETY: old is a valid layout.
        let ptr = unsafe { allocator.alloc(old) };
        assert!(!ptr.is_null());
        // SAFETY: ptr was returned for old.
        unsafe { ptr.write(0x5a) };
        // SAFETY: matching realloc/dealloc.
        let grown = unsafe { allocator.realloc(ptr, old, new.size()) };
        assert!(!grown.is_null());
        assert_eq!(unsafe { grown.read() }, 0x5a);
        unsafe { allocator.dealloc(grown, new) };
    }

    #[test]
    fn realloc_repeated_in_class_hits_lookup() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: valid layout.
        let mut ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null());
        for _ in 0..8 {
            // SAFETY: ptr is the live allocation from the previous step.
            let next = unsafe { allocator.realloc(ptr, layout, layout.size()) };
            assert_eq!(next, ptr);
            ptr = next;
        }
        // SAFETY: final pointer is still live for layout.
        unsafe { allocator.dealloc(ptr, layout) };
    }

    #[test]
    fn realloc_to_extent_uses_pagemap() {
        let allocator = Allocator::new();
        let small = Layout::from_size_align(64, 8).unwrap();
        let large = Layout::from_size_align(128 * 1024, 8).unwrap();
        // SAFETY: valid layouts.
        let ptr = unsafe { allocator.alloc(small) };
        assert!(!ptr.is_null());
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let pages = Allocator::ctx().expect("allocator ctx").pages;
        assert!(matches!(
            pages.get(NonNull::new(grown).unwrap()),
            Some(PageOwner::Extent(_))
        ));
        unsafe { allocator.dealloc(grown, large) };
    }

    #[test]
    fn adopt_then_owner_local_free() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, second, run) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let first = bind_alloc_small(tls, &ctx, layout);
            let second = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, first);
            unbind(tls);
            (id, first, second, run)
        };

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(
            Allocator::free_remote(&ctx, PageOwner::Run(run), first),
            Ok(())
        );
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
        assert_eq!(THREAD_HEAP.free_run(run, second), Ok(()));
        THREAD_HEAP.retire_adopted(&ctx);
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn adopt_race_one_winner() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, addrs, run_addr) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let live = alloc_live(tls, &ctx, layout, 2);
            let run = run_of(pages, live[0]);
            let addrs: Vec<usize> = live.iter().map(|p| p.as_ptr() as usize).collect();
            unbind(tls);
            (id, addrs, run.as_ptr() as usize)
        };

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        let (tx, rx) = mpsc::channel();
        let hold = Barrier::new(2);
        thread::scope(|scope| {
            for addr in addrs {
                let tx = tx.clone();
                let hold = &hold;
                scope.spawn(move || {
                    let run = NonNull::new(run_addr as *mut Run).unwrap();
                    let ptr = NonNull::new(addr as *mut u8).unwrap();
                    assert_eq!(
                        Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                        Ok(())
                    );
                    tx.send(THREAD_HEAP.adopted_id_for_test()).unwrap();
                    hold.wait();
                    unbind(&THREAD_HEAP);
                });
            }
            drop(tx);
            let winners: Vec<_> = rx.iter().flatten().collect();
            assert_eq!(winners.len(), 1);
        });
        assert!(
            ctx.heaps.get(id).is_none()
                || ctx.heaps.get(id).map(Heap::mode) == Some(HeapMode::Draining)
        );
    }

    #[test]
    fn adopter_exit_retires_adopted_heap() {
        let allocator = Allocator::new();
        let ctx = install(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, second, run) = {
            let tls = &THREAD_HEAP;
            let id = tls.bind(&ctx).unwrap();
            let first = bind_alloc_small(tls, &ctx, layout);
            let second = bind_alloc_small(tls, &ctx, layout);
            let run = run_of(pages, first);
            unbind(tls);
            (id, first, second, run)
        };

        let run_addr = run.as_ptr() as usize;
        let first_addr = first.as_ptr() as usize;
        thread::scope(|scope| {
            scope.spawn(|| {
                let run = NonNull::new(run_addr as *mut Run).unwrap();
                let first = NonNull::new(first_addr as *mut u8).unwrap();
                assert_eq!(
                    Allocator::free_remote(&ctx, PageOwner::Run(run), first),
                    Ok(())
                );
                assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
                unbind(&THREAD_HEAP);
            });
        });

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(
            ctx.heaps.free(id, PageOwner::Run(run), second, &ctx),
            Ok(())
        );
        assert!(ctx.heaps.get(id).is_none());
    }
}
