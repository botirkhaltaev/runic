use core::{mem::MaybeUninit, ptr::NonNull, slice};

use crate::{
    extent::{Extent, ExtentId},
    os_memory::OsMemory,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentTableError {
    InvalidReservation,
    Occupied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentReservation {
    id: ExtentId,
}

impl ExtentReservation {
    pub(crate) const fn id(self) -> ExtentId {
        self.id
    }
}

pub(crate) struct ExtentTable {
    slots: Option<ExtentSlots>,
    next: u32,
}

impl ExtentTable {
    #[cfg(not(test))]
    pub(crate) const MAX_EXTENTS: usize = 65_536;

    #[cfg(test)]
    pub(crate) const MAX_EXTENTS: usize = 1024;

    pub(crate) const fn new() -> Self {
        Self {
            slots: None,
            next: 0,
        }
    }

    pub(crate) fn reserve(&mut self) -> Option<ExtentReservation> {
        let start = usize::try_from(self.next).ok()?;

        for offset in 0..Self::MAX_EXTENTS {
            let sum = start.checked_add(offset)?;
            let index = if sum >= Self::MAX_EXTENTS {
                sum.checked_sub(Self::MAX_EXTENTS)?
            } else {
                sum
            };

            let slot = self.slots_mut()?.get_mut(index)?;

            if slot.reserve() {
                let next = if index + 1 == Self::MAX_EXTENTS {
                    0
                } else {
                    index.checked_add(1)?
                };
                self.next = u32::try_from(next).ok()?;
                let id = ExtentId::new(u32::try_from(index).ok()?)?;

                return Some(ExtentReservation { id });
            }
        }

        None
    }

    pub(crate) fn release(&mut self, reservation: ExtentReservation) {
        let Some(slot) = self.slot_mut(reservation.id) else {
            return;
        };

        slot.release();
    }

    pub(crate) fn insert(
        &mut self,
        reservation: ExtentReservation,
        extent: Extent,
    ) -> Result<ExtentId, ExtentTableError> {
        if reservation.id != extent.id() {
            return Err(ExtentTableError::InvalidReservation);
        }

        let Some(slot) = self.slot_mut(reservation.id) else {
            return Err(ExtentTableError::InvalidReservation);
        };

        if slot.insert(extent) {
            Ok(reservation.id)
        } else {
            Err(ExtentTableError::Occupied)
        }
    }

    pub(crate) fn get(&self, id: ExtentId) -> Option<&Extent> {
        self.slot(id)?.get()
    }

    pub(crate) fn remove(&mut self, id: ExtentId) -> Option<Extent> {
        self.slot_mut(id)?.remove()
    }

    fn slots(&self) -> Option<&ExtentSlots> {
        self.slots.as_ref()
    }

    fn slots_mut(&mut self) -> Option<&mut ExtentSlots> {
        if self.slots.is_none() {
            self.slots = Some(ExtentSlots::new(Self::MAX_EXTENTS)?);
        }

        self.slots.as_mut()
    }

    fn slot(&self, id: ExtentId) -> Option<&ExtentSlot> {
        self.slots()?.get(Self::index(id)?)
    }

    fn slot_mut(&mut self, id: ExtentId) -> Option<&mut ExtentSlot> {
        self.slots.as_mut()?.get_mut(Self::index(id)?)
    }

    fn index(id: ExtentId) -> Option<usize> {
        usize::try_from(id.get()).ok()
    }
}

struct ExtentSlots {
    mapping: crate::os_memory::Mapping,
    slots: NonNull<ExtentSlot>,
}

// SAFETY: ExtentSlots owns mmap-backed slots. Moving ownership to another
// thread does not permit concurrent mutation of allocator metadata.
unsafe impl Send for ExtentSlots {}

impl ExtentSlots {
    fn new(len: usize) -> Option<Self> {
        let byte_len = len.checked_mul(core::mem::size_of::<ExtentSlot>())?;
        let mapping = OsMemory::map(byte_len)?;
        let slots = mapping.base().cast::<ExtentSlot>();

        Some(Self { mapping, slots })
    }

    fn get(&self, index: usize) -> Option<&ExtentSlot> {
        self.slots().get(index)
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut ExtentSlot> {
        self.slots_mut().get_mut(index)
    }

    fn slots(&self) -> &[ExtentSlot] {
        let len = self.mapping.range().len() / core::mem::size_of::<ExtentSlot>();

        // SAFETY: slots points to mmap storage sized for len ExtentSlot entries.
        unsafe { slice::from_raw_parts(self.slots.as_ptr(), len) }
    }

    fn slots_mut(&mut self) -> &mut [ExtentSlot] {
        let len = self.mapping.range().len() / core::mem::size_of::<ExtentSlot>();

        // SAFETY: ExtentSlots has unique access to the mmap storage here.
        unsafe { slice::from_raw_parts_mut(self.slots.as_ptr(), len) }
    }
}

impl Drop for ExtentSlots {
    fn drop(&mut self) {
        for slot in self.slots_mut() {
            slot.drop_extent();
        }
    }
}

#[repr(C)]
struct ExtentSlot {
    extent: MaybeUninit<Extent>,
    state: SlotState,
}

impl ExtentSlot {
    fn reserve(&mut self) -> bool {
        if !self.state.is_empty() {
            return false;
        }

        self.state = SlotState::reserved();
        true
    }

    fn release(&mut self) {
        if self.state.is_reserved() {
            self.state = SlotState::empty();
        }
    }

    fn insert(&mut self, extent: Extent) -> bool {
        if !self.state.is_reserved() {
            return false;
        }

        self.extent.write(extent);
        self.state = SlotState::occupied();
        true
    }

    fn get(&self) -> Option<&Extent> {
        if !self.state.is_occupied() {
            return None;
        }

        // SAFETY: occupied state is set only after extent.write initializes the slot.
        Some(unsafe { self.extent.assume_init_ref() })
    }

    fn remove(&mut self) -> Option<Extent> {
        if !self.state.is_occupied() {
            return None;
        }

        self.state = SlotState::empty();

        // SAFETY: occupied state was true on entry, so the slot contains an initialized Extent.
        Some(unsafe { self.extent.assume_init_read() })
    }

    fn drop_extent(&mut self) {
        let _ = self.remove();
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct SlotState(u8);

impl SlotState {
    const EMPTY: u8 = 0;
    const RESERVED: u8 = 1;
    const OCCUPIED: u8 = 2;

    const fn empty() -> Self {
        Self(Self::EMPTY)
    }

    const fn reserved() -> Self {
        Self(Self::RESERVED)
    }

    const fn occupied() -> Self {
        Self(Self::OCCUPIED)
    }

    const fn is_empty(self) -> bool {
        self.0 == Self::EMPTY
    }

    const fn is_reserved(self) -> bool {
        self.0 == Self::RESERVED
    }

    const fn is_occupied(self) -> bool {
        self.0 == Self::OCCUPIED
    }
}
