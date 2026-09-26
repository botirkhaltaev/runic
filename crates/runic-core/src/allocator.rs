use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    config::{AllocatorConfig, HugePage, Numa},
    heap::{
        AllocatorCtx, ExtentInit, HeapError, Heaps, Run, THREAD_HEAPS, ThreadFreeError,
        extent::config::ExtentConfig, run::config::RunConfig,
    },
    layout::LayoutSpec,
    memory::{Memory, Os, PageMap, PageOwner},
    size_class::{SizeClass, SizeClasses},
};

pub struct Allocator {
    config: AllocatorConfig,
    from_env: bool,
}

/// mmap payload for [`AllocatorCtx`]. Not returned to callers.
struct Process {
    pages: PageMap,
    heaps: Heaps,
}

static PROCESS: AtomicPtr<Process> = AtomicPtr::new(core::ptr::null_mut());

impl Allocator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: AllocatorConfig::new(),
            from_env: false,
        }
    }

    /// Construct the C-preload entry that overlays `RUNIC_*` at first `init`.
    ///
    /// Const `with_*` calls set the baseline; recognized environment values
    /// override that setting. Regular [`Self::new`] never reads the environment.
    #[must_use]
    pub const fn preload() -> Self {
        Self {
            config: AllocatorConfig::new(),
            from_env: true,
        }
    }

    /// First `init` in the process wins; later configs are ignored.
    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            config,
            from_env: false,
        }
    }

    #[must_use]
    pub const fn with_hugepage(mut self, hugepage: HugePage) -> Self {
        self.config = self.config.with_hugepage(hugepage);
        self
    }

    #[must_use]
    pub const fn with_numa(mut self, numa: Numa) -> Self {
        self.config = self.config.with_numa(numa);
        self
    }

    #[must_use]
    pub const fn with_extent_config(mut self, extent: ExtentConfig) -> Self {
        self.config = self.config.with_extent_config(extent);
        self
    }

    #[must_use]
    pub const fn with_run_config(mut self, run: RunConfig) -> Self {
        self.config = self.config.with_run_config(run);
        self
    }

    /// Installed pages and heaps, or `None` before first `init`.
    #[inline]
    pub(crate) fn ctx() -> Option<AllocatorCtx<'static>> {
        let process_ptr = NonNull::new(PROCESS.load(Ordering::Acquire))?;
        // SAFETY: installed payload lives for the process lifetime.
        let process = unsafe { process_ptr.as_ref() };
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
            if let Some(ptr) = THREAD_HEAPS.alloc(class) {
                return ptr.as_ptr();
            }
            return self.alloc_miss(class);
        }
        let Some(ctx) = self.ctx_or_init() else {
            return null_mut();
        };
        Self::alloc_extent(&ctx, spec, ExtentInit::Uninit)
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
        let spec = LayoutSpec::from_layout(layout);
        let Some(live) = NonNull::new(ptr) else {
            Self::abort();
        };
        if let Some(class) = SizeClasses::class_for(spec)
            && THREAD_HEAPS.free(live, class).is_some()
        {
            return;
        }
        Self::dealloc_slow(live, spec);
    }

    /// Pointer-only free for callers without a `Layout` (the C ABI).
    ///
    /// The owner comes from `PageMap`, never `Run::header_of`: an extent
    /// mapping need not cover the run-header address, so that probe can fault
    /// on a pointer whose size class is unknown. Null is forbidden here; the C
    /// boundary no-ops `free(NULL)`. Unknown pointers abort.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer previously returned by this allocator.
    pub unsafe fn free(&self, ptr: *mut u8) {
        let Some(live) = NonNull::new(ptr) else {
            Self::abort();
        };
        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        let Some(owner) = ctx.pages.get(live) else {
            Self::abort();
        };
        if let PageOwner::Run(run) = owner
            && THREAD_HEAPS.free(live, run.class()).is_some()
        {
            return;
        }
        match THREAD_HEAPS.free_owner(owner, live, &ctx) {
            Ok(()) => {}
            Err(error) => Self::free_fail(&ctx, live, error),
        }
    }

    /// Usable size of a live allocation (`malloc_usable_size`).
    ///
    /// Null is 0; unknown pointers abort.
    #[must_use]
    pub fn usable_size(&self, ptr: *mut u8) -> usize {
        let Some(live) = NonNull::new(ptr) else {
            return 0;
        };
        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        match ctx.pages.get(live) {
            Some(owner) => owner.usable(),
            None => Self::abort(),
        }
    }

    /// Layout-known owner: in-page header for a size class, else `PageMap`.
    ///
    /// Pointer-only free and resize must not call this. `header_of` loads the
    /// run-header page, which an extent mapping may omit.
    pub(crate) fn lookup(pages: &PageMap, ptr: NonNull<u8>, spec: LayoutSpec) -> Option<PageOwner> {
        if SizeClasses::class_for(spec).is_some()
            && let Some(run) = Run::header_of(ptr)
        {
            return Some(PageOwner::Run(run));
        }
        pages.get(ptr)
    }

    /// Pointer-only realloc. Owner from `PageMap`, never `Run::header_of`.
    ///
    /// `new` is the requested layout of the replacement (`max_align_t` at the C
    /// boundary). Null `ptr` is forbidden here; the C boundary maps
    /// `realloc(NULL, n)` to `malloc`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer previously returned by this allocator.
    #[must_use]
    pub unsafe fn resize(&self, ptr: *mut u8, new: Layout) -> *mut u8 {
        let Some(live) = NonNull::new(ptr) else {
            Self::abort();
        };
        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        let Some(owner) = ctx.pages.get(live) else {
            Self::abort();
        };
        let old_len = owner.usable();
        let new_spec = LayoutSpec::from_layout(new);
        match owner.resize_in_place(live, new_spec) {
            Ok(true) => return ptr,
            Ok(false) => {}
            Err(_) => Self::abort(),
        }

        self.replace(ptr, old_len, new, OldFree::Pointer)
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
        let Ok(new_layout) = Layout::from_size_align(new_size, old.align()) else {
            return null_mut();
        };
        let Some(old_ptr) = NonNull::new(ptr) else {
            Self::abort();
        };
        let old_spec = LayoutSpec::from_layout(old);
        let Some(owner) = Self::lookup(ctx.pages, old_ptr, old_spec) else {
            Self::abort();
        };

        match owner.resize_in_place(old_ptr, LayoutSpec::from_layout(new_layout)) {
            Ok(true) => return ptr,
            Ok(false) => {}
            Err(_) => Self::abort(),
        }

        self.replace(ptr, old.size(), new_layout, OldFree::Layout(old))
    }

    /// Allocate `new_layout`, copy `old_len` bytes, then free `ptr`.
    fn replace(&self, ptr: *mut u8, old_len: usize, new_layout: Layout, free: OldFree) -> *mut u8 {
        // SAFETY: `new_layout` is well-formed. The pointer is used only when non-null.
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return null_mut();
        }
        // SAFETY: `new_ptr` is fresh for `new_layout`. `ptr` is live for `old_len` bytes.
        unsafe { copy_nonoverlapping(ptr, new_ptr, old_len.min(new_layout.size())) };
        match free {
            // SAFETY: `ptr` is still the live original.
            OldFree::Pointer => unsafe { self.free(ptr) },
            // SAFETY: `ptr` was allocated with `old`.
            OldFree::Layout(old) => unsafe { self.dealloc(ptr, old) },
        }
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
            let Some(ctx) = self.ctx_or_init() else {
                return null_mut();
            };
            return Self::alloc_extent(&ctx, spec, ExtentInit::Zeroed);
        };

        let ptr = if let Some(hit) = THREAD_HEAPS.alloc(class) {
            hit.as_ptr()
        } else {
            self.alloc_miss(class)
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
    pub(crate) fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }

    #[cold]
    fn init(&self) -> Option<AllocatorCtx<'static>> {
        let config = if self.from_env {
            self.config.overlay_env()
        } else {
            self.config
        };
        let mapping = Os::map(core::mem::size_of::<Process>())?;
        let process = mapping.base().cast::<Process>();
        // SAFETY: `process` is uniquely owned page-aligned mmap. Fields are
        // written before the CAS publishes the pointer.
        unsafe {
            process.as_ptr().write(Process {
                pages: PageMap::new(),
                heaps: Heaps::new(config),
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

    fn ctx_or_init(&self) -> Option<AllocatorCtx<'static>> {
        Self::ctx().or_else(|| self.init())
    }

    /// Not owner-local on TLS: bind Active heap, then flush-then-alloc (run or extent).
    #[cold]
    fn bind_alloc(ctx: &AllocatorCtx<'static>, request: AllocKind) -> *mut u8 {
        if THREAD_HEAPS.bind(ctx).is_none() {
            return null_mut();
        }
        let allocated = match request {
            AllocKind::Run(class) => THREAD_HEAPS.alloc_miss(class, ctx),
            AllocKind::Extent(spec, init) => THREAD_HEAPS.alloc_extent(spec, init, ctx),
        };
        match allocated {
            Ok(Some(ptr)) => ptr.as_ptr(),
            Ok(None) => null_mut(),
            Err(_) => Self::abort(),
        }
    }

    /// Cross-heap free: adopt a Draining heap, else Active claim → enqueue,
    /// else `Heaps::free` under Draining.
    ///
    /// Coalescing is by owner inbox. `Remote` callers only — heap-domain errors abort
    /// in `dealloc` before this runs.
    #[cold]
    fn free_remote(
        ctx: &AllocatorCtx,
        owner: PageOwner,
        ptr: NonNull<u8>,
    ) -> Result<(), HeapError> {
        let heap = owner.heap();

        let heap_id = if let Some(id) = heap.active_id() {
            id
        } else {
            if THREAD_HEAPS.adopt(heap, ctx) {
                return THREAD_HEAPS
                    .free_owner(owner, ptr, ctx)
                    .map_err(|free| match free {
                        ThreadFreeError::Heap(error) => error,
                        ThreadFreeError::Remote(_) => HeapError::InvalidMetadata,
                    });
            }
            let heap_id = heap.id();
            match ctx.heaps.free(heap_id, owner, ptr, ctx) {
                Ok(()) => return Ok(()),
                Err(HeapError::InvalidHeap) => {}
                Err(error) => return Err(error),
            }
            heap.active_id().ok_or(HeapError::InvalidMetadata)?
        };

        match owner {
            PageOwner::Run(run) => {
                run.claim(ptr)?;
            }
            PageOwner::Extent(extent) => {
                extent.claim(ptr)?;
            }
        }

        loop {
            match heap.enqueue(heap_id, owner) {
                Ok(()) => return Ok(()),
                Err(HeapError::InvalidHeap) => {}
                Err(error) => return Err(error),
            }

            match ctx.heaps.flush(heap_id, ctx, Some(owner)) {
                Ok(()) => return Ok(()),
                Err(HeapError::InvalidHeap) => {}
                Err(error) => return Err(error),
            }

            if !heap.matches(heap_id) {
                return Ok(());
            }
        }
    }

    /// Current-run miss, large, or unbound: `lookup` then typed free.
    #[inline(never)]
    fn dealloc_slow(ptr: NonNull<u8>, spec: LayoutSpec) {
        let Some(ctx) = Self::ctx() else {
            Self::abort();
        };
        match THREAD_HEAPS.free_slow(ptr, spec, &ctx) {
            Ok(()) => {}
            Err(error) => Self::free_fail(&ctx, ptr, error),
        }
    }

    /// Current-run empty, unbound, or state not yet installed.
    #[inline(never)]
    fn alloc_miss(&self, class: SizeClass) -> *mut u8 {
        let Some(ctx) = self.ctx_or_init() else {
            return null_mut();
        };
        match THREAD_HEAPS.alloc_miss(class, &ctx) {
            Ok(Some(ptr)) => return ptr.as_ptr(),
            Ok(None) => {}
            Err(_) => Self::abort(),
        }
        Self::bind_alloc(&ctx, AllocKind::Run(class))
    }

    /// Bound-extent miss: TLS extent alloc, else bind.
    #[inline(never)]
    fn alloc_extent(ctx: &AllocatorCtx<'static>, spec: LayoutSpec, init: ExtentInit) -> *mut u8 {
        match THREAD_HEAPS.alloc_extent(spec, init, ctx) {
            Ok(Some(ptr)) => return ptr.as_ptr(),
            Ok(None) => {}
            Err(_) => Self::abort(),
        }
        Self::bind_alloc(ctx, AllocKind::Extent(spec, init))
    }

    /// Cross-heap or domain-error after the TLS hit missed.
    #[cold]
    fn free_fail(ctx: &AllocatorCtx, ptr: NonNull<u8>, error: ThreadFreeError) {
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

/// How [`Allocator::replace`] frees the old block.
#[derive(Clone, Copy)]
enum OldFree {
    Pointer,
    Layout(Layout),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::thread::ThreadHeaps;
    use crate::heap::{Extent, Heap, HeapMode, Run};
    use std::sync::{Barrier, mpsc};
    use std::thread;

    fn ctx(allocator: &Allocator) -> AllocatorCtx<'static> {
        allocator
            .init()
            .or_else(Allocator::ctx)
            .expect("allocator ctx")
    }

    fn alloc_small(tls: &ThreadHeaps, ctx: &AllocatorCtx, layout: Layout) -> NonNull<u8> {
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        if let Some(ptr) = tls.alloc(class) {
            return ptr;
        }
        tls.alloc_miss(class, ctx).unwrap().unwrap()
    }

    fn alloc_extent(
        tls: &ThreadHeaps,
        ctx: &AllocatorCtx,
        layout: Layout,
        init: ExtentInit,
    ) -> NonNull<u8> {
        let spec = LayoutSpec::from_layout(layout);
        tls.alloc_extent(spec, init, ctx).unwrap().unwrap()
    }

    fn run_of(pages: &PageMap, ptr: NonNull<u8>) -> &Run {
        let PageOwner::Run(run) = pages.get(ptr).unwrap() else {
            panic!("expected a run-owned pointer");
        };
        run
    }

    fn extent_of(pages: &PageMap, ptr: NonNull<u8>) -> &Extent {
        let PageOwner::Extent(extent) = pages.get(ptr).unwrap() else {
            panic!("expected an extent-owned pointer");
        };
        extent
    }

    fn alloc_live(
        tls: &ThreadHeaps,
        ctx: &AllocatorCtx,
        layout: Layout,
        n: u32,
    ) -> Vec<NonNull<u8>> {
        (0..n).map(|_| alloc_small(tls, ctx, layout)).collect()
    }

    fn free_all(tls: &ThreadHeaps, pages: &'static PageMap, ptrs: &[NonNull<u8>]) {
        for &ptr in ptrs {
            assert_eq!(tls.free_run(run_of(pages, ptr), ptr), Ok(()));
        }
    }

    #[test]
    fn current_run_free_hits_without_lookup() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            assert_eq!(tls.free(ptr, class), Some(()));
            assert_eq!(tls.alloc(class), Some(ptr));
            assert_eq!(tls.free(ptr, class), Some(()));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn bind_reuses_active_slot() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let tls = &THREAD_HEAPS;
        tls.unbind(&ctx);

        let id = tls.bind(&ctx).unwrap();
        assert_eq!(tls.bind(&ctx), Some(id));
        tls.unbind(&ctx);
    }

    #[test]
    fn adopted_second_slot_unbinds_without_releasing_bound_current() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        let tls = &THREAD_HEAPS;
        tls.unbind(&ctx);

        let bound = tls.bind(&ctx).unwrap();
        let local = alloc_small(tls, &ctx, layout);
        assert_eq!(tls.free(local, class), Some(()));

        let (remote_id, remote_addr) = thread::scope(|scope| {
            scope
                .spawn(|| {
                    let remote = &THREAD_HEAPS;
                    remote.unbind(&ctx);
                    let id = remote.bind(&ctx).unwrap();
                    let ptr = alloc_small(remote, &ctx, layout);
                    remote.unbind(&ctx);
                    (id, ptr.as_ptr().expose_provenance())
                })
                .join()
                .unwrap()
        });
        let remote = NonNull::new(core::ptr::with_exposed_provenance_mut(remote_addr)).unwrap();
        let owner = ctx.pages.get(remote).unwrap();

        assert_eq!(Allocator::free_remote(&ctx, owner, remote), Ok(()));
        assert!(ctx.heaps.get(remote_id).is_none());
        assert_eq!(tls.bind(&ctx), Some(bound));
        assert_eq!(tls.alloc(class), Some(local));
        assert_eq!(tls.free(local, class), Some(()));
        tls.unbind(&ctx);
    }

    #[test]
    fn owner_free_keeps_last_attached_heap() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let tls = &THREAD_HEAPS;
        tls.unbind(&ctx);

        let id = tls.bind(&ctx).unwrap();
        let ptr = alloc_small(tls, &ctx, layout);
        let owner = ctx.pages.get(ptr).unwrap();
        assert_eq!(tls.free_owner(owner, ptr, &ctx), Ok(()));
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
        assert_eq!(tls.bind(&ctx), Some(id));
        tls.unbind(&ctx);
    }

    #[test]
    fn owner_free_publishes_immediately() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            assert!(!run.is_live());
            assert_eq!(run.allocate(), Some(ptr));
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn current_run_switches_when_full() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let first = alloc_small(tls, &ctx, layout);
            let run_a = run_of(pages, first);
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut ptrs = Vec::with_capacity(capacity + 1);
            ptrs.push(first);
            for _ in 1..capacity {
                ptrs.push(alloc_small(tls, &ctx, layout));
            }
            assert!(run_a.is_full());
            let extra = alloc_small(tls, &ctx, layout);
            let run_b = run_of(pages, extra);
            assert!(run_a != run_b);
            ptrs.push(extra);
            free_all(tls, pages, &ptrs);
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn free_to_non_current_full_run_relinks_available() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut a_ptrs = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                a_ptrs.push(alloc_small(tls, &ctx, layout));
            }
            let run_a = run_of(pages, a_ptrs[0]);
            assert!(run_a.is_full());
            let b = alloc_small(tls, &ctx, layout);
            let run_b = run_of(pages, b);
            assert!(run_a != run_b);
            assert_eq!(tls.free_run(run_a, a_ptrs[0]), Ok(()));
            let mut b_ptrs = vec![b];
            for _ in 1..capacity {
                b_ptrs.push(alloc_small(tls, &ctx, layout));
            }
            assert!(run_b.is_full());
            let reused = alloc_small(tls, &ctx, layout);
            assert_eq!(reused, a_ptrs[0]);
            assert!(run_of(pages, reused) == run_a);
            assert_eq!(tls.free_run(run_a, reused), Ok(()));
            free_all(tls, pages, &a_ptrs[1..]);
            free_all(tls, pages, &b_ptrs);
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn header_of_finds_in_page_run_after_free() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let spec = LayoutSpec::from_layout(layout);
            let Some(PageOwner::Run(run)) = Allocator::lookup(ctx.pages, ptr, spec) else {
                panic!("alloc should publish a run");
            };
            assert!(Run::header_of(ptr).unwrap() == run);
            assert_eq!(tls.free_slow(ptr, spec, &ctx), Ok(()));
            let again = alloc_small(tls, &ctx, layout);
            assert_eq!(again, ptr);
            assert!(Run::header_of(again).unwrap() == run);
            assert_eq!(tls.free_run(run, again), Ok(()));
            tls.unbind(&ctx);
            assert_eq!(
                Allocator::lookup(ctx.pages, ptr, spec),
                Some(PageOwner::Run(run))
            );
        };
    }

    #[test]
    fn unbind_with_current_runs_leaves_exact_live() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, ptr, run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert!(run.is_live());
            tls.unbind(&ctx);
            (id, ptr, run)
        };
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert!(run.is_live());
        assert_eq!(ctx.heaps.free(id, PageOwner::Run(run), ptr, &ctx), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn remote_claim_accept_publishes_once() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // User-held block; claim is the remote admission path.
            assert_eq!(run.claim(ptr), Ok(()));
            assert_eq!(run.accept(), crate::heap::Accept::Done);
            assert_eq!(run.allocate(), Some(ptr));
            assert!(run.free(ptr).is_ok());
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_extent_free_keeps_page_entry_while_cached() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            assert_eq!(pages.get(ptr), Some(PageOwner::Extent(extent)));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_allocates_small_from_current_heap() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(run.heap().id(), id);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_allocates_extent_from_current_heap() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            assert_eq!(extent.heap().id(), id);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_rejects_duplicate_remote_free() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            // Heap stays Active (still bound). free_remote is the cross-thread path —
            // claim+enqueue twice must report DoubleFree on the second claim.
            assert_eq!(
                Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                Ok(())
            );
            assert_eq!(
                Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                Err(HeapError::DoubleFree)
            );
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn retained_remote_claim_completes_under_draining() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(run.claim(ptr), Ok(()));
            tls.unbind(&ctx);
            (id, run)
        };

        assert_eq!(ctx.heaps.unbind(id, &ctx), Ok(()));
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(ctx.heaps.flush(id, &ctx, Some(PageOwner::Run(run))), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn remote_frees_to_distinct_heaps_publish_independently_without_batching() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let (ready_a, wait_a) = mpsc::channel::<Vec<usize>>();
        let (ready_b, wait_b) = mpsc::channel::<Vec<usize>>();
        let (go_a, start_a) = mpsc::channel::<()>();
        let (go_b, start_b) = mpsc::channel::<()>();
        let (done_a, finished_a) = mpsc::channel::<bool>();
        let (done_b, finished_b) = mpsc::channel::<bool>();

        thread::scope(|scope| {
            scope.spawn(move || {
                {
                    let tls = &THREAD_HEAPS;
                    let id = tls.bind(&ctx).unwrap();
                    let live = alloc_live(tls, &ctx, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_a
                        .send(
                            live.iter()
                                .map(|p| p.as_ptr().expose_provenance())
                                .collect(),
                        )
                        .unwrap();
                    start_a.recv().unwrap();
                    let heap = ctx.heaps.get(id).unwrap();
                    let mut inner = heap.require_inner();
                    assert_eq!(heap.flush(&mut inner, &ctx, None), Ok(()));
                    done_a.send(run.is_live()).unwrap();
                    drop(inner);
                    tls.unbind(&ctx);
                };
            });
            scope.spawn(move || {
                {
                    let tls = &THREAD_HEAPS;
                    let id = tls.bind(&ctx).unwrap();
                    let live = alloc_live(tls, &ctx, layout, 8);
                    let run = run_of(pages, live[0]);
                    ready_b
                        .send(
                            live.iter()
                                .map(|p| p.as_ptr().expose_provenance())
                                .collect(),
                        )
                        .unwrap();
                    start_b.recv().unwrap();
                    let heap = ctx.heaps.get(id).unwrap();
                    let mut inner = heap.require_inner();
                    assert_eq!(heap.flush(&mut inner, &ctx, None), Ok(()));
                    done_b.send(run.is_live()).unwrap();
                    drop(inner);
                    tls.unbind(&ctx);
                };
            });

            let addrs_a = wait_a.recv().unwrap();
            let addrs_b = wait_b.recv().unwrap();
            let ptr_a = NonNull::new(core::ptr::with_exposed_provenance_mut(addrs_a[0])).unwrap();
            let ptr_b = NonNull::new(core::ptr::with_exposed_provenance_mut(addrs_b[0])).unwrap();
            let run_a = run_of(pages, ptr_a);
            let run_b = run_of(pages, ptr_b);
            for addr in addrs_a {
                let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap();
                assert_eq!(
                    Allocator::free_remote(&ctx, PageOwner::Run(run_a), ptr),
                    Ok(())
                );
            }
            for addr in addrs_b {
                let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap();
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
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let class = SizeClasses::class_for(LayoutSpec::from_layout(
            Layout::from_size_align(64, 8).unwrap(),
        ))
        .unwrap();

        let (id, run, addrs) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let mut addrs = Vec::with_capacity(THREADS * PER_THREAD);
            for _ in 0..THREADS * PER_THREAD {
                let ptr = match tls.alloc(class) {
                    Some(ptr) => ptr,
                    None => tls.alloc_miss(class, &ctx).unwrap().unwrap(),
                };
                addrs.push(ptr.as_ptr().expose_provenance());
            }
            let run = run_of(
                pages,
                NonNull::new(core::ptr::with_exposed_provenance_mut(addrs[0])).unwrap(),
            );
            (id, run, addrs)
        };

        let heap = ctx.heaps.get(id).unwrap();
        let live = addrs.as_slice();

        thread::scope(|scope| {
            for t in 0..THREADS {
                scope.spawn(move || {
                    let start = t * PER_THREAD;
                    for &addr in &live[start..start + PER_THREAD] {
                        let ptr =
                            NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap();
                        run.claim(ptr).unwrap();
                        assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
                    }
                });
            }
        });

        assert_eq!(heap.leases(), 0);
        {
            let tls = &THREAD_HEAPS;
            let mut inner = heap.require_inner();
            assert_eq!(heap.flush(&mut inner, &ctx, None), Ok(()));
            assert!(!run.is_live());
            drop(inner);
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn flush_accepts_nonempty_run_inbox_before_reclaim() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let id = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let live = alloc_live(tls, &ctx, layout, 8);
            let run = run_of(pages, live[0]);
            for ptr in live {
                run.claim(ptr).unwrap();
            }
            let heap = ctx.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Run(run)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        };

        assert_eq!(ctx.heaps.flush(id, &ctx, None), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
        THREAD_HEAPS.unbind(&ctx);
    }

    #[test]
    fn flush_accepts_nonempty_extent_inbox_before_reclaim() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        let id = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_extent(tls, &ctx, layout, ExtentInit::Uninit);
            let extent = extent_of(pages, ptr);
            extent.claim(ptr).unwrap();
            let heap = ctx.heaps.get(id).unwrap();
            assert_eq!(heap.enqueue(id, PageOwner::Extent(extent)), Ok(()));
            assert_eq!(heap.close(id), Ok(()));
            id
        };

        assert_eq!(ctx.heaps.flush(id, &ctx, None), Ok(()));
        assert!(ctx.heaps.get(id).is_none());
        THREAD_HEAPS.unbind(&ctx);
    }

    #[test]
    fn allocator_tracks_live_run_allocations_through_draining_reclaim() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, first_run, second, second_run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let first = alloc_small(tls, &ctx, layout);
            let second = alloc_small(tls, &ctx, layout);
            let first_run = run_of(pages, first);
            let second_run = run_of(pages, second);
            tls.unbind(&ctx);
            (id, first, first_run, second, second_run)
        };

        // unbind already closed Active; heap should be Draining with live blocks.
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
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr, run) = {
            let tls = &THREAD_HEAPS;
            let heap = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            tls.unbind(&ctx);
            (heap, ptr, run)
        };

        assert_eq!(ctx.heaps.free(heap, PageOwner::Run(run), ptr, &ctx), Ok(()));
        assert!(pages.get(ptr).is_some());
        {
            let tls = &THREAD_HEAPS;
            let reused = tls.bind(&ctx).unwrap();
            if reused.index() == heap.index() {
                assert_ne!(reused.generation(), heap.generation());
            }
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_release_retains_empty_heap_run_page_entry_for_reuse() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (heap, ptr) = {
            let tls = &THREAD_HEAPS;
            let heap = tls.bind(&ctx).unwrap();
            let ptr = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, ptr);
            assert_eq!(tls.free_run(run, ptr), Ok(()));
            assert!(pages.get(ptr).is_some());
            tls.unbind(&ctx);
            (heap, ptr)
        };

        assert!(pages.get(ptr).is_some());

        {
            let tls = &THREAD_HEAPS;
            let reused = tls.bind(&ctx).unwrap();
            if reused.index() == heap.index() {
                assert_ne!(reused.generation(), heap.generation());
            }
            let reused_ptr = alloc_small(tls, &ctx, layout);
            assert!(pages.get(ptr).is_some());
            assert!(pages.get(reused_ptr).is_some());
            let reused_run = run_of(pages, reused_ptr);
            assert_eq!(tls.free_run(reused_run, reused_ptr), Ok(()));
            tls.unbind(&ctx);
        };
    }

    #[test]
    fn allocator_zeroed_large_allocation_uses_current_heap() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let ptr = alloc_extent(tls, &ctx, layout, ExtentInit::Zeroed);
            // SAFETY: ptr was just allocated zeroed for layout.
            assert!(
                unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) }
                    .iter()
                    .all(|&byte| byte == 0)
            );
            let extent = extent_of(pages, ptr);
            assert_eq!(extent.heap().id(), id);
            assert_eq!(tls.free_extent(extent, ptr, &ctx), Ok(()));
            tls.unbind(&ctx);
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
        let id = run_of(pages, NonNull::new(ptr).unwrap()).heap().id();

        // SAFETY: ptr was returned by alloc(small) above and is not yet freed.
        let grown = unsafe { allocator.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        let extent = extent_of(pages, NonNull::new(grown).unwrap());

        // SAFETY: PageMap stores only live extent pointers.
        assert_eq!(extent.heap().id(), id);

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
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (first0, rest, extra, run_a, run_b) = {
            let tls = &THREAD_HEAPS;
            let _id = tls.bind(&ctx).unwrap();
            let capacity = crate::heap::run::RUN_SIZE / 64;
            let mut first = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                first.push(alloc_small(tls, &ctx, layout));
            }
            let extra = alloc_small(tls, &ctx, layout);
            let run_a = run_of(pages, first[0]);
            let run_b = run_of(pages, extra);
            assert!(run_a != run_b);
            let first0 = first[0];
            let rest = first[1..].to_vec();
            (first0, rest, extra, run_a, run_b)
        };
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        assert_eq!(THREAD_HEAPS.free(first0, class), None);
        // SAFETY: first0 is a live block on the non-current full run.
        unsafe { allocator.dealloc(first0.as_ptr(), layout) };
        {
            let tls = &THREAD_HEAPS;
            assert_eq!(
                Allocator::lookup(pages, first0, LayoutSpec::from_layout(layout)),
                Some(PageOwner::Run(run_a))
            );
            assert_eq!(tls.free_run(run_b, extra), Ok(()));
            free_all(tls, pages, &rest);
            tls.unbind(&ctx);
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
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, second, run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let first = alloc_small(tls, &ctx, layout);
            let second = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, first);
            tls.unbind(&ctx);
            (id, first, second, run)
        };

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(
            Allocator::free_remote(&ctx, PageOwner::Run(run), first),
            Ok(())
        );
        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
        assert_eq!(THREAD_HEAPS.free_run(run, second), Ok(()));
        THREAD_HEAPS.unbind(&ctx);
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn concurrent_frees_complete_across_adoption() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, addrs, run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let live = alloc_live(tls, &ctx, layout, 2);
            let run = run_of(pages, live[0]);
            let addrs: Vec<usize> = live
                .iter()
                .map(|p| p.as_ptr().expose_provenance())
                .collect();
            tls.unbind(&ctx);
            (id, addrs, run)
        };

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        let (tx, rx) = mpsc::channel();
        let hold = Barrier::new(2);
        thread::scope(|scope| {
            let posted = &tx;
            let gate = &hold;
            for addr in addrs {
                scope.spawn(move || {
                    let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap();
                    assert_eq!(
                        Allocator::free_remote(&ctx, PageOwner::Run(run), ptr),
                        Ok(())
                    );
                    posted.send(()).unwrap();
                    gate.wait();
                    THREAD_HEAPS.unbind(&ctx);
                });
            }
            assert!(rx.recv().is_ok());
            assert!(rx.recv().is_ok());
        });
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn adopter_exit_unbinds_adopted_heap() {
        let allocator = Allocator::new();
        let ctx = ctx(&allocator);
        let pages = ctx.pages;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let (id, first, second, run) = {
            let tls = &THREAD_HEAPS;
            let id = tls.bind(&ctx).unwrap();
            let first = alloc_small(tls, &ctx, layout);
            let second = alloc_small(tls, &ctx, layout);
            let run = run_of(pages, first);
            tls.unbind(&ctx);
            (id, first, second, run)
        };

        let first_addr = first.as_ptr().expose_provenance();
        thread::scope(|scope| {
            scope.spawn(|| {
                let first =
                    NonNull::new(core::ptr::with_exposed_provenance_mut(first_addr)).unwrap();
                assert_eq!(
                    Allocator::free_remote(&ctx, PageOwner::Run(run), first),
                    Ok(())
                );
                assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
                THREAD_HEAPS.unbind(&ctx);
            });
        });

        assert_eq!(ctx.heaps.get(id).map(Heap::mode), Some(HeapMode::Draining));
        assert_eq!(
            ctx.heaps.free(id, PageOwner::Run(run), second, &ctx),
            Ok(())
        );
        assert!(ctx.heaps.get(id).is_none());
    }

    #[test]
    fn free_recovers_run_class() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: valid layout.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null());
        let class = SizeClasses::class_for(LayoutSpec::from_layout(layout)).unwrap();
        assert_eq!(allocator.usable_size(ptr), class.size());
        // SAFETY: ptr was returned by alloc.
        unsafe { allocator.free(ptr) };
    }

    #[test]
    fn free_recovers_extent_len() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
        // SAFETY: valid extent layout.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(allocator.usable_size(ptr), layout.size());
        // SAFETY: ptr was returned by alloc.
        unsafe { allocator.free(ptr) };
    }

    #[test]
    fn usable_size_null_is_zero() {
        let allocator = Allocator::new();
        assert_eq!(allocator.usable_size(core::ptr::null_mut()), 0);
    }

    #[test]
    fn resize_overaligned_extent_preserves_prefix() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(32, 8192).unwrap();
        // SAFETY: valid extent layout.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { ptr.write(0xa5) };
        let new = Layout::from_size_align(64, 16).unwrap();
        // SAFETY: ptr came from alloc; resize recovers the owner via PageMap.
        let grown = unsafe { allocator.resize(ptr, new) };
        assert!(!grown.is_null());
        assert_eq!(unsafe { grown.read() }, 0xa5);
        // SAFETY: grown is live.
        unsafe { allocator.free(grown) };
    }

    #[cfg(feature = "safe")]
    #[test]
    fn free_accepts_live_blocks_whose_first_word_looks_like_a_link() {
        let allocator = Allocator::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: valid layout.
        let (first, second) = unsafe { (allocator.alloc(layout), allocator.alloc(layout)) };
        assert!(!first.is_null() && !second.is_null());
        // SAFETY: both blocks are live and at least one word long.
        unsafe {
            let link = second.addr().to_ne_bytes();
            first.copy_from_nonoverlapping(link.as_ptr(), link.len());
            second.write_bytes(0, link.len());
            allocator.free(first);
            allocator.free(second);
        }
    }
}
