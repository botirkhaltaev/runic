use core::ptr::NonNull;

/// Ownership-free byte geometry: a `(base, len)` view with no mmap lifecycle.
///
/// Used for run payload spans, extent user sub-ranges, and other non-owning
/// views. mmap ownership lives on [`crate::memory::Mapping`]. Run publish takes
/// the 64 KiB payload [`AddressRange`]; extent publish still takes `&Mapping`.
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
            .checked_add(range.len())
            .is_some_and(|end| end <= self.len())
    }
}
