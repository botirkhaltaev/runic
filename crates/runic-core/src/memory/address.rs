use core::ptr::NonNull;

#[derive(Clone, Copy)]
pub(crate) struct AddressRange {
    base: NonNull<u8>,
    len: usize,
}

impl AddressRange {
    pub(crate) const fn new(base: NonNull<u8>, len: usize) -> Self {
        Self { base, len }
    }

    pub(crate) const fn base(self) -> NonNull<u8> {
        self.base
    }

    pub(crate) const fn len(self) -> usize {
        self.len
    }

    pub(crate) fn offset_of(self, ptr: NonNull<u8>) -> Option<usize> {
        let offset = ptr.as_ptr().addr().checked_sub(self.base.as_ptr().addr())?;

        if offset < self.len {
            Some(offset)
        } else {
            None
        }
    }

    pub(crate) fn contains(self, range: Self) -> bool {
        let Some(offset) = self.offset_of(range.base) else {
            return false;
        };

        offset
            .checked_add(range.len)
            .is_some_and(|end| end <= self.len)
    }
}
