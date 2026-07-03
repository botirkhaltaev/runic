use core::alloc::Layout;

use spin::Mutex;

use crate::state::State;

static STATE: Mutex<State> = Mutex::new(State::new());

pub struct Allocator;

impl Allocator {
    /// Allocates memory for `layout` using the process-global Ferralloc state.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw, uninitialized memory. The caller must use it
    /// only according to `layout`, avoid out-of-bounds access, and eventually
    /// pass the same pointer and a compatible layout back to this allocator.
    pub unsafe fn alloc(layout: Layout) -> *mut u8 {
        Self::with(|state| state.alloc(layout))
    }

    /// Deallocates memory previously returned by this allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `layout`. Passing an unknown pointer, an interior pointer, or an
    /// incompatible layout violates the allocator contract and may abort.
    pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
        Self::with(|state| state.dealloc(ptr, layout));
    }

    /// Changes the size of an allocation using allocate-copy-free semantics.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a pointer previously returned by this allocator
    /// for `old`. If a non-null pointer is supplied, no other live reference may
    /// be used to access the old allocation after successful reallocation.
    pub unsafe fn realloc(ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        Self::with(|state| state.realloc(ptr, old, new_size))
    }

    /// Allocates zero-initialized memory for `layout`.
    ///
    /// # Safety
    ///
    /// The returned pointer is raw memory. The caller must use it only according
    /// to `layout` and eventually pass it back to this allocator with a
    /// compatible layout.
    pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
        Self::with(|state| state.alloc_zeroed(layout))
    }

    fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = STATE.lock();
        f(&mut state)
    }
}
