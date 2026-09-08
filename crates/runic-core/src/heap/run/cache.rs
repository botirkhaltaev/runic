use core::{cell::Cell, ptr::NonNull};

use super::{RUN_SIZE, Run};

/// One-entry TLS payload-range cache. Own-heap runs only; extents never stored.
///
/// Empty is `base == usize::MAX`. `ptr & !(RUN_SIZE-1)` is always a multiple of
/// `RUN_SIZE`, never `MAX`, so an empty cache cannot hit.
pub(crate) struct RunCache {
    base: Cell<usize>,
    run: Cell<*mut Run>,
}

impl RunCache {
    pub(crate) const fn new() -> Self {
        Self {
            base: Cell::new(usize::MAX),
            run: Cell::new(core::ptr::null_mut()),
        }
    }

    /// Payload-range hit. No run-null test. Bases are `RUN_SIZE`-aligned.
    #[inline]
    pub(crate) fn hit(&self, ptr: NonNull<u8>) -> Option<NonNull<Run>> {
        if ptr.as_ptr().addr() & !(RUN_SIZE - 1) != self.base.get() {
            return None;
        }
        // SAFETY: a matching range is stored only with a live own-heap run.
        Some(unsafe { NonNull::new_unchecked(self.run.get()) })
    }

    /// Remember `run`'s payload range. Caller checked own-heap.
    pub(crate) fn store(&self, run: NonNull<Run>) {
        // SAFETY: PageMap / arena supply only live run pointers.
        let run_ref = unsafe { run.as_ref() };
        self.run.set(run.as_ptr());
        self.base.set(run_ref.range().base().as_ptr().addr());
    }

    pub(crate) fn clear(&self) {
        self.base.set(usize::MAX);
        self.run.set(core::ptr::null_mut());
    }
}
