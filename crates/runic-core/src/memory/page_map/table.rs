use core::{
    cell::UnsafeCell,
    mem::{MaybeUninit, size_of},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::memory::{Mapping, OsMemory};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError, SPAN_SLOTS,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment},
    span::{SpanRecord, SpanSlot},
};

#[repr(C)]
pub(super) struct L1Table {
    pub(super) entries: [L1Entry; L1_ENTRIES],
}

impl L1Table {
    pub(super) fn page_entry(&self, l1_index: L1Index, l2_index: L2Index) -> Option<MapEntry> {
        self.entries.get(l1_index.get())?.page_entry(l2_index)
    }

    pub(super) fn entry(&self, index: L1Index) -> Result<&L1Entry, PageMapError> {
        self.entries
            .get(index.get())
            .ok_or(PageMapError::InvalidRange)
    }

    pub(super) fn ensure_l2_table(&self, index: L1Index) -> Result<(), PageMapError> {
        let entry = self.entry(index)?;

        if entry.has_l2_table() {
            return Ok(());
        }

        let Some(mapping) = OsMemory::map(size_of::<L2Table>()) else {
            return Err(PageMapError::MetadataAllocFailed);
        };
        entry.install_l2_mapping(mapping);

        Ok(())
    }
}

#[repr(C)]
pub(super) struct L1Entry {
    table: AtomicPtr<L2Table>,
    mapping: UnsafeCell<MaybeUninit<Mapping>>,
    occupied_pages: AtomicU32,
}

// SAFETY: L1Entry publishes the L2 table pointer atomically. Mapping storage is
// initialized before publication and dropped only when PageMap is dropped.
unsafe impl Sync for L1Entry {}

impl L1Entry {
    pub(super) fn has_l2_table(&self) -> bool {
        !self.table.load(Ordering::Acquire).is_null()
    }

    fn l2_table(&self) -> Option<NonNull<L2Table>> {
        NonNull::new(self.table.load(Ordering::Acquire))
    }

    pub(super) fn l2_table_ref(&self) -> Option<&L2Table> {
        let table = self.l2_table()?;

        // SAFETY: l2_table returns the live L2 table pointer owned by this L1 entry.
        Some(unsafe { table.as_ref() })
    }

    fn install_l2_mapping(&self, mapping: Mapping) {
        let table = mapping.base().cast::<L2Table>().as_ptr();
        // SAFETY: mutation is serialized by the allocator lifecycle lock, and readers cannot
        // observe this mapping until table is published below.
        unsafe { (*self.mapping.get()).write(mapping) };
        self.occupied_pages.store(0, Ordering::Release);
        self.table.store(table, Ordering::Release);
    }

    pub(super) fn drop_l2_mapping(&mut self) {
        if self.table.load(Ordering::Acquire).is_null() {
            return;
        }

        self.table.store(core::ptr::null_mut(), Ordering::Release);
        self.occupied_pages.store(0, Ordering::Release);
        // SAFETY: PageMap drop has unique access; mapping was initialized before table publication.
        unsafe { self.mapping.get_mut().assume_init_drop() };
    }

    pub(super) fn owns_segment(
        &self,
        segment: L2Segment,
        expected: MapEntry,
    ) -> Result<bool, PageMapError> {
        let Some(table) = self.l2_table_ref() else {
            return Ok(expected.is_empty());
        };

        if expected.is_empty() && self.occupied_pages.load(Ordering::Acquire) == 0 {
            return Ok(true);
        }

        table.owns_segment(segment, expected)
    }

    fn page_entry(&self, index: L2Index) -> Option<MapEntry> {
        self.l2_table_ref()?.get(index)
    }

    pub(super) fn assign_direct(
        &self,
        segment: L2Segment,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .load(Ordering::Acquire)
            .checked_add(segment.pages())
            .ok_or(PageMapError::InvalidRange)?;
        let table = self
            .l2_table_ref()
            .ok_or(PageMapError::MetadataAllocFailed)?;

        table.assign_direct(segment, value)?;
        self.occupied_pages.store(occupied_pages, Ordering::Release);

        Ok(())
    }

    pub(super) fn assign_span(
        &self,
        segment: L2Segment,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .load(Ordering::Acquire)
            .checked_add(segment.pages())
            .ok_or(PageMapError::InvalidRange)?;
        let table = self
            .l2_table_ref()
            .ok_or(PageMapError::MetadataAllocFailed)?;

        table.assign_span(segment, value)?;
        self.occupied_pages.store(occupied_pages, Ordering::Release);

        Ok(())
    }

    pub(super) fn clear_segment(&self, segment: L2Segment) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .load(Ordering::Acquire)
            .checked_sub(segment.pages())
            .ok_or(PageMapError::UnexpectedEntry)?;
        let table = self.l2_table_ref().ok_or(PageMapError::UnexpectedEntry)?;

        table.clear_segment(segment)?;
        self.occupied_pages.store(occupied_pages, Ordering::Release);

        Ok(())
    }
}

#[repr(C)]
pub(super) struct L2Table {
    pub(super) pages: [AtomicMapEntry; L2_ENTRIES],
    pub(super) spans: [SpanSlot; SPAN_SLOTS],
}

impl L2Table {
    fn get(&self, index: L2Index) -> Option<MapEntry> {
        let page = self.pages.get(index.get())?.load();
        if !page.is_empty() {
            return Some(page);
        }

        self.spans
            .iter()
            .find_map(|slot| slot.record_containing(index).map(SpanRecord::entry))
    }

    fn owns_segment(&self, segment: L2Segment, expected: MapEntry) -> Result<bool, PageMapError> {
        if expected.is_empty() {
            return Ok(self.segment_is_free(segment));
        }

        let pages = self
            .pages
            .get(segment.range())
            .ok_or(PageMapError::InvalidRange)?;
        if pages.iter().all(|entry| entry.load() == expected) {
            return Ok(true);
        }

        Ok(self
            .spans
            .iter()
            .any(|slot| slot.matches(segment, expected)))
    }

    fn assign_direct(&self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        self.write_pages(segment, value)
    }

    fn assign_span(&self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        if self.install_span(segment, value) {
            return Ok(());
        }

        self.write_pages(segment, value)
    }

    fn clear_segment(&self, segment: L2Segment) -> Result<(), PageMapError> {
        if self.clear_span(segment) {
            return Ok(());
        }

        self.write_pages(segment, MapEntry::empty())
    }

    fn segment_is_free(&self, segment: L2Segment) -> bool {
        let Some(pages) = self.pages.get(segment.range()) else {
            return false;
        };

        pages.iter().all(|entry| entry.load().is_empty())
            && self.spans.iter().all(|slot| !slot.overlaps(segment))
    }

    fn install_span(&self, segment: L2Segment, entry: MapEntry) -> bool {
        let record = SpanRecord::new(segment, entry);
        let Some(slot) = self.spans.iter().find(|slot| slot.is_empty()) else {
            return false;
        };

        slot.set(record);
        true
    }

    fn clear_span(&self, segment: L2Segment) -> bool {
        let Some(slot) = self.spans.iter().find(|slot| slot.covers(segment)) else {
            return false;
        };

        slot.clear();
        true
    }

    fn write_pages(&self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        let entries = self
            .pages
            .get(segment.range())
            .ok_or(PageMapError::InvalidRange)?;

        for entry in entries {
            entry.store(value);
        }

        Ok(())
    }
}
