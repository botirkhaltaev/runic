use core::{
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

pub(super) struct RemoteBlocks {
    head: AtomicPtr<u8>,
}

impl RemoteBlocks {
    pub(super) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    pub(super) fn push(&self, ptr: NonNull<u8>) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            Self::write_next(ptr, NonNull::new(head));
            match self.head.compare_exchange_weak(
                head,
                ptr.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    pub(super) fn take_all(&self) -> Option<NonNull<u8>> {
        if self.head.load(Ordering::Acquire).is_null() {
            return None;
        }

        NonNull::new(self.head.swap(core::ptr::null_mut(), Ordering::AcqRel))
    }

    pub(super) fn next(ptr: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: remote queue links are stored only in remotely freed blocks until owner drain.
        NonNull::new(unsafe { ptr.cast::<*mut u8>().as_ptr().read() })
    }

    fn write_next(ptr: NonNull<u8>, next: Option<NonNull<u8>>) {
        // SAFETY: remote queue links are stored only in remotely freed blocks until owner drain.
        unsafe {
            ptr.cast::<*mut u8>()
                .as_ptr()
                .write(next.map_or(core::ptr::null_mut(), NonNull::as_ptr));
        }
    }
}
