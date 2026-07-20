use core::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeapId {
    slot: NonZeroU32,
    generation: NonZeroU32,
}

impl HeapId {
    pub(crate) fn new(slot: u32, generation: NonZeroU32) -> Option<Self> {
        Some(Self {
            slot: NonZeroU32::new(slot.checked_add(1)?)?,
            generation,
        })
    }

    pub(crate) const fn index(self) -> u32 {
        self.slot.get() - 1
    }

    pub(crate) const fn generation(self) -> NonZeroU32 {
        self.generation
    }
}
