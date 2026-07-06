use core::ptr::NonNull;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ZeroStatus {
    KnownZeroed,
    NeedsZeroing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Allocation {
    ptr: NonNull<u8>,
    zero_status: ZeroStatus,
}

impl Allocation {
    pub(crate) const fn new(ptr: NonNull<u8>, zero_status: ZeroStatus) -> Self {
        Self { ptr, zero_status }
    }

    pub(crate) const fn ptr(self) -> NonNull<u8> {
        self.ptr
    }

    pub(crate) const fn zero_status(self) -> ZeroStatus {
        self.zero_status
    }
}
