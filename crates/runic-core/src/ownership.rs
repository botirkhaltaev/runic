use core::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeapId(NonZeroU32);

impl HeapId {
    pub(crate) fn from_index(index: u32) -> Option<Self> {
        NonZeroU32::new(index.checked_add(1)?).map(Self)
    }

    pub(crate) const fn index(self) -> u32 {
        self.0.get() - 1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunOwner {
    Shared,
    Thread(HeapId),
}
