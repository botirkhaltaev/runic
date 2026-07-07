use core::alloc::Layout;

use spin::Mutex;

use crate::{
    config::AllocatorConfig, heap::SharedHeap, layout::LayoutSpec, local, size_class::SizeClasses,
};

pub struct Allocator {
    shared: Mutex<SharedHeap>,
}

impl Allocator {
    #[must_use]
    pub const fn new() -> Self {
        Self::with_config(AllocatorConfig::new())
    }

    #[must_use]
    pub const fn with_config(config: AllocatorConfig) -> Self {
        Self {
            shared: Mutex::new(SharedHeap::with_config(config)),
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
        if let Some(class) = SizeClasses::id_for(spec) {
            let key = self.shared_key();
            if !local::accepts_local(key) {
                let mut shared = self.shared.lock();
                return shared.alloc(layout);
            }

            if let Some(Some(ptr)) = local::with_local(key, |heap| heap.allocate(class)) {
                return ptr.as_ptr();
            }

            let mut shared = self.shared.lock();
            let current_heap = local::with_local(key, |heap| heap.id());
            let Some(heap_id) = current_heap.or_else(|| shared.acquire_heap_id()) else {
                return core::ptr::null_mut();
            };

            if current_heap.is_some() {
                if shared.drain_remote(heap_id).is_err() {
                    return core::ptr::null_mut();
                }

                if let Some(Some(ptr)) = local::with_local(key, |heap| heap.allocate(class)) {
                    return ptr.as_ptr();
                }
            }

            let Some((run, allocation)) = shared.refill_local(heap_id, class) else {
                return core::ptr::null_mut();
            };

            let _ = local::with_local_or_init(key, heap_id, |heap| {
                heap.attach_registered_run(run);
                // SAFETY: refill_local returns a stable RunArena pointer.
                if unsafe { run.as_ref() }.has_available_blocks() {
                    heap.push_available(class, run);
                }
            });

            return allocation.ptr().as_ptr();
        }

        let mut shared = self.shared.lock();
        shared.alloc(layout)
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `layout`. Passing an unknown pointer, an interior pointer, or an
    /// incompatible layout violates the allocator contract and may abort.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(ptr) = core::ptr::NonNull::new(ptr) {
            let spec = LayoutSpec::from_layout(layout);
            if let Some(class) = SizeClasses::id_for(spec) {
                match local::with_local(self.shared_key(), |heap| heap.free_local(class, ptr)) {
                    Some(Ok(true)) => return,
                    Some(Err(_)) => Self::abort(),
                    Some(Ok(false)) | None => {}
                }
            }
        }

        let current_heap = local::with_local(self.shared_key(), |heap| heap.id());
        let mut shared = self.shared.lock();
        if shared.dealloc(ptr, layout, current_heap).is_err() {
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
        let mut shared = self.shared.lock();
        shared
            .realloc(ptr, old, new_size)
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
        let mut shared = self.shared.lock();
        shared.alloc_zeroed(layout)
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }

    fn shared_key(&self) -> *const () {
        core::ptr::from_ref(&self.shared).cast()
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        let key = self.shared_key();
        let Some(heap) = local::take_local(key) else {
            return;
        };

        let mut shared = self.shared.lock();
        shared.retire_local_heap(heap.id());
    }
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}
