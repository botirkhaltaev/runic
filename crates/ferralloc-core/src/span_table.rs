use core::mem::MaybeUninit;

use crate::span::{Span, SpanId};

pub(crate) struct SpanTable {
    entries: [MaybeUninit<Span>; Self::MAX_SPANS],
    occupied: [bool; Self::MAX_SPANS],
    reserved: [bool; Self::MAX_SPANS],
    next: u32,
}

impl SpanTable {
    #[cfg(not(test))]
    pub(crate) const MAX_SPANS: usize = 65_536;

    #[cfg(test)]
    pub(crate) const MAX_SPANS: usize = 1024;

    pub(crate) const fn new() -> Self {
        Self {
            entries: [const { MaybeUninit::uninit() }; Self::MAX_SPANS],
            occupied: [false; Self::MAX_SPANS],
            reserved: [false; Self::MAX_SPANS],
            next: 0,
        }
    }

    pub(crate) fn reserve_id(&mut self) -> Option<SpanId> {
        for offset in 0..Self::MAX_SPANS {
            let index = (self.next as usize + offset) % Self::MAX_SPANS;

            if !self.occupied[index] && !self.reserved[index] {
                self.reserved[index] = true;
                self.next = ((index + 1) % Self::MAX_SPANS) as u32;
                return SpanId::new(index as u32);
            }
        }

        None
    }

    pub(crate) fn release_id(&mut self, id: SpanId) {
        let index = id.get() as usize;

        if index < Self::MAX_SPANS && !self.occupied[index] {
            self.reserved[index] = false;
        }
    }

    pub(crate) fn insert(&mut self, id: SpanId, span: Span) -> bool {
        let index = id.get() as usize;

        if index >= Self::MAX_SPANS || self.occupied[index] || !self.reserved[index] {
            return false;
        }

        self.entries[index].write(span);
        self.occupied[index] = true;
        true
    }

    pub(crate) fn get(&self, id: SpanId) -> Option<&Span> {
        let index = id.get() as usize;

        if index >= Self::MAX_SPANS || !self.occupied[index] {
            return None;
        }

        Some(unsafe { self.entries[index].assume_init_ref() })
    }

    pub(crate) fn get_mut(&mut self, id: SpanId) -> Option<&mut Span> {
        let index = id.get() as usize;

        if index >= Self::MAX_SPANS || !self.occupied[index] {
            return None;
        }

        Some(unsafe { self.entries[index].assume_init_mut() })
    }

    pub(crate) unsafe fn remove(&mut self, id: SpanId) -> Option<Span> {
        let index = id.get() as usize;

        if index >= Self::MAX_SPANS || !self.occupied[index] {
            return None;
        }

        self.occupied[index] = false;
        self.reserved[index] = false;
        Some(unsafe { self.entries[index].assume_init_read() })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        layout::LayoutSpec,
        os_memory::OsMemory,
        size_class::SizeClasses,
        span::{SPAN_SIZE, Span},
    };

    use super::*;

    fn small_span(id: SpanId) -> (Span, crate::os_memory::Mapping) {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let class = SizeClasses::new().get(spec).unwrap();

        (Span::small(id, mapping, class), mapping)
    }

    fn table() -> Box<SpanTable> {
        Box::new(SpanTable::new())
    }

    #[test]
    fn span_table_reserves_ids_from_zero() {
        let mut table = table();

        assert_eq!(table.reserve_id().unwrap().get(), 0);
        assert_eq!(table.reserve_id().unwrap().get(), 1);
    }

    #[test]
    fn span_table_release_id_makes_reserved_slot_available() {
        let mut table = table();
        let first = table.reserve_id().unwrap();
        let second = table.reserve_id().unwrap();

        table.release_id(first);

        assert_eq!(second.get(), 1);
        for expected in 2..SpanTable::MAX_SPANS as u32 {
            assert_eq!(table.reserve_id().unwrap().get(), expected);
        }
        assert_eq!(table.reserve_id().unwrap(), first);
    }

    #[test]
    fn span_table_insert_get_round_trip() {
        let mut table = table();
        let id = table.reserve_id().unwrap();
        let (span, mapping) = small_span(id);

        assert!(table.insert(id, span));
        assert_eq!(table.get(id).unwrap().id(), id);

        let span = unsafe { table.remove(id) }.unwrap();
        assert_eq!(span.id(), id);
        unsafe { OsMemory::new().unmap(mapping) };
    }

    #[test]
    fn span_table_rejects_occupied_slot() {
        let mut table = table();
        let id = table.reserve_id().unwrap();
        let (first, first_mapping) = small_span(id);
        let (second, second_mapping) = small_span(id);

        assert!(table.insert(id, first));
        assert!(!table.insert(id, second));

        let _ = unsafe { table.remove(id) };
        unsafe {
            OsMemory::new().unmap(first_mapping);
            OsMemory::new().unmap(second_mapping);
        }
    }

    #[test]
    fn span_table_rejects_unreserved_insert() {
        let mut table = table();
        let id = SpanId::new(0).unwrap();
        let (span, mapping) = small_span(id);

        assert!(!table.insert(id, span));

        unsafe { OsMemory::new().unmap(mapping) };
    }

    #[test]
    fn span_table_get_mut_allows_span_mutation() {
        let mut table = table();
        let id = table.reserve_id().unwrap();
        let (span, mapping) = small_span(id);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();

        assert!(table.insert(id, span));
        let ptr = table.get_mut(id).unwrap().take_block(spec).unwrap();

        assert!(table.get(id).unwrap().block_index(ptr).is_some());

        let _ = unsafe { table.remove(id) };
        unsafe { OsMemory::new().unmap(mapping) };
    }

    #[test]
    fn span_table_remove_clears_slot() {
        let mut table = table();
        let id = table.reserve_id().unwrap();
        let (span, mapping) = small_span(id);

        assert!(table.insert(id, span));
        assert!(unsafe { table.remove(id) }.is_some());
        assert!(table.get(id).is_none());
        assert!(table.get_mut(id).is_none());

        unsafe { OsMemory::new().unmap(mapping) };
    }
}
