use core::{
    marker::PhantomData,
    ptr::{NonNull, addr_of_mut},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use crate::layout::Header;

/// Exclusive drain of one CPU slab. `Drop` publishes `current` and restores capacity.
pub struct Quiesced<'a, T> {
    header: *mut Header,
    cap: u32,
    current: u32,
    unlock: Option<&'a AtomicBool>,
    _t: PhantomData<(&'a T, *const ())>,
}

impl<'a, T> Quiesced<'a, T> {
    /// # Safety
    /// Exclusive mutator of this slab until drop. `current` is the live length.
    pub(crate) unsafe fn new(
        header: *mut Header,
        cap: u32,
        current: u32,
        unlock: Option<&'a AtomicBool>,
    ) -> Self {
        Self {
            header,
            cap,
            current,
            unlock,
            _t: PhantomData,
        }
    }

    /// Pop one remaining pointer.
    #[must_use]
    pub fn pop(&mut self) -> Option<NonNull<T>> {
        if self.current == 0 {
            return None;
        }
        self.current -= 1;
        // SAFETY: we are the exclusive mutator; slots follow `Header`.
        #[allow(clippy::cast_ptr_alignment)]
        Some(unsafe {
            self.header
                .add(1)
                .cast::<NonNull<T>>()
                .add(self.current as usize)
                .read()
        })
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
