use core::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeapId {
    slot: NonZeroU32,
    generation: NonZeroU32,
}

impl HeapId {
    pub(crate) const ROOT: Self = Self {
        slot: NonZeroU32::MAX,
        generation: NonZeroU32::MIN,
    };

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

    pub(crate) const fn is_root(self) -> bool {
        self.slot.get() == Self::ROOT.slot.get()
            && self.generation.get() == Self::ROOT.generation.get()
    }
}
