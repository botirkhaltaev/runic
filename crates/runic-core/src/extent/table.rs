use crate::{
    extent::{Extent, ExtentId},
    slot_store::{SlotStore, SlotStoreError},
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
    slots: SlotStore<Extent>,
}

// SAFETY: ExtentTable owns allocator metadata accessed through the global heap lock.
// Moving ownership to another thread does not permit concurrent metadata mutation.
unsafe impl Send for ExtentTable {}

impl ExtentTable {
    pub(crate) const fn new(capacity: u32) -> Self {
        Self {
            slots: SlotStore::new(capacity),
        }
    }

    pub(crate) fn reserve(&mut self) -> Option<ExtentReservation> {
        let index = self.slots.reserve()?;
        let Some(id) = Self::id(index) else {
            let _ = self.slots.release(index);
            return None;
        };

        Some(ExtentReservation { id })
    }

    pub(crate) fn release(&mut self, reservation: ExtentReservation) {
        let Some(index) = Self::index(reservation.id) else {
            return;
        };

        let _ = self.slots.release(index);
    }

    pub(crate) fn insert(
        &mut self,
        reservation: ExtentReservation,
        extent: Extent,
    ) -> Result<ExtentId, ExtentTableError> {
        if reservation.id != extent.id() {
            self.release(reservation);
            return Err(ExtentTableError::InvalidReservation);
        }

        let Some(index) = Self::index(reservation.id) else {
            return Err(ExtentTableError::InvalidReservation);
        };

        self.slots
            .insert(index, extent)
            .map_err(ExtentTableError::from)?;

        Ok(reservation.id)
    }

    pub(crate) fn get(&self, id: ExtentId) -> Option<&Extent> {
        self.slots.get(Self::index(id)?)
    }

    pub(crate) fn get_mut(&mut self, id: ExtentId) -> Option<&mut Extent> {
        self.slots.get_mut(Self::index(id)?)
    }

    pub(crate) fn remove(&mut self, id: ExtentId) -> Option<Extent> {
        self.slots.remove(Self::index(id)?)
    }

    fn index(id: ExtentId) -> Option<usize> {
        usize::try_from(id.index()).ok()
    }

    fn id(index: usize) -> Option<ExtentId> {
        ExtentId::from_index(u32::try_from(index).ok()?)
    }
}

impl From<SlotStoreError> for ExtentTableError {
    fn from(error: SlotStoreError) -> Self {
        match error {
            SlotStoreError::InvalidIndex | SlotStoreError::NotReserved => Self::InvalidReservation,
            SlotStoreError::Occupied => Self::Occupied,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        extent::{Extent, ExtentId},
        layout::LayoutSpec,
        memory::OsMemory,
    };

    use super::*;

    fn reusable_extent(id: ExtentId) -> Extent {
        let spec = LayoutSpec::from_size_align(65_536, 8).unwrap();
        let len = spec.mapping_len(OsMemory::page_size()).unwrap();
        let mapping = OsMemory::map(len).unwrap();

        Extent::new(id, mapping, spec).unwrap()
    }

    fn table_with_capacity(capacity: usize) -> ExtentTable {
        ExtentTable::new(u32::try_from(capacity).unwrap())
    }

    #[test]
    fn extent_table_zero_capacity_reserves_none() {
        let mut table = ExtentTable::new(0);

        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn extent_table_respects_injected_capacity() {
        let mut table = table_with_capacity(2);

        assert_eq!(table.reserve().unwrap().id().index(), 0);
        assert_eq!(table.reserve().unwrap().id().index(), 1);
        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn extent_table_insert_get_round_trip() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let extent = reusable_extent(reservation.id());

        let id = table.insert(reservation, extent).unwrap();
        assert_eq!(table.get(id).unwrap().id(), id);

        let extent = table.remove(id).unwrap();
        assert_eq!(extent.id(), id);
    }

    #[test]
    fn extent_table_invalid_insert_releases_reservation() {
        let mut table = table_with_capacity(4);
        let reservation = table.reserve().unwrap();
        let released = reservation.id();
        let wrong_id = ExtentId::from_index(released.index() + 1).unwrap();
        let extent = reusable_extent(wrong_id);

        assert_eq!(
            table.insert(reservation, extent),
            Err(ExtentTableError::InvalidReservation)
        );

        for expected in 1..4 {
            assert_eq!(table.reserve().unwrap().id().index(), expected);
        }
        assert_eq!(table.reserve().unwrap().id(), released);
    }
}
