use core::{
    ptr::{NonNull, addr_of_mut},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use crate::layout::{self, Header};

/// Exclusive drain of one CPU slab. `Drop` publishes `current` and restores capacity.
/// `*mut` fields keep this `!Send`.
pub struct Quiesced<'a, T> {
    header: *mut Header,
    slots: *mut NonNull<T>,
    cap: u32,
    current: u32,
    unlock: Option<&'a AtomicBool>,
}

impl<'a, T> Quiesced<'a, T> {
    /// # Safety
    /// Exclusive mutator of this slab until drop. `current` is the live length.
    pub(crate) unsafe fn new(
        header: NonNull<Header>,
        cap: u32,
        current: u32,
        unlock: Option<&'a AtomicBool>,
    ) -> Self {
        Self {
            header: header.as_ptr(),
            slots: layout::slots(header),
            cap,
            current,
            unlock,
        }
    }

    /// Pop one remaining pointer.
    #[must_use]
    pub fn pop(&mut self) -> Option<NonNull<T>> {
        if self.current == 0 {
            return None;
        }
        self.current -= 1;
        // SAFETY: exclusive mutator; `current` is in range.
        Some(unsafe { self.slots.add(self.current as usize).read() })
    }

    /// Drain remaining pointers.
    pub fn drain(&mut self) -> impl Iterator<Item = NonNull<T>> + '_ {
        core::iter::from_fn(move || self.pop())
    }
}

impl<T> Iterator for Quiesced<'_, T> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.current as usize;
        (n, Some(n))
    }
}

impl<T> Drop for Quiesced<'_, T> {
    fn drop(&mut self) {
        // SAFETY: exclusive after stop + fence (or TAS); header is live.
        unsafe {
            (*self.header).current = self.current;
            AtomicU32::from_ptr(addr_of_mut!((*self.header).capacity))
                .store(self.cap, Ordering::Release);
        }
        if let Some(flag) = self.unlock {
            flag.store(false, Ordering::Release);
        }
    }
}
