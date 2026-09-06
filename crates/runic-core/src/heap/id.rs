use core::num::NonZeroU32;

#[derive(Clone, Copy, Debug)]
#[repr(C, align(8))]
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

    /// One-word identity: `slot | generation << 32`.
    #[inline]
    fn word(self) -> u64 {
        u64::from(self.slot.get()) | (u64::from(self.generation.get()) << 32)
    }
}

impl PartialEq for HeapId {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.word() == other.word()
    }
}

impl Eq for HeapId {}
