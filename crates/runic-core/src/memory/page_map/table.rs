use core::{
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    allocator::Allocator,
    memory::{Mapping, OsMemory},
};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment},
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
}

#[repr(C)]
pub(super) struct L1Entry {
    table: AtomicPtr<L2Table>,
    /// Once installed, retained until `PageMap` drop. Written only by the CAS winner of
    /// [`Self::install_l2`]; read only under exclusive `PageMap::Drop`.
    mapping: UnsafeCell<Option<Mapping>>,
}

// SAFETY: `table` is published atomically for lock-free get. `mapping` is written once by the
// install CAS winner and read only on exclusive `PageMap` drop — `get` never touches it.
unsafe impl Sync for L1Entry {}

impl L1Entry {
    pub(super) fn l2_table_ref(&self) -> Option<&L2Table> {
        let table = NonNull::new(self.table.load(Ordering::Acquire))?;

        // SAFETY: `table` is the live L2 pointer owned by this L1 entry for the PageMap lifetime.
        Some(unsafe { table.as_ref() })
    }

    /// Once-only L2 install: mmap, CAS-publish the table pointer, winner stores `Mapping`.
    pub(super) fn install_l2(&self) -> Result<&L2Table, PageMapError> {
        if let Some(table) = self.l2_table_ref() {
            return Ok(table);
        }

        let mapping =
            OsMemory::map(size_of::<L2Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
        let ptr = mapping.base().cast::<L2Table>().as_ptr();

        match self.table.compare_exchange(
            core::ptr::null_mut(),
            ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: this thread won the null→ptr CAS; sole writer of `mapping`.
                unsafe {
                    *self.mapping.get() = Some(mapping);
                }
            }
            Err(_) => {
                drop(mapping);
            }
        }

        self.l2_table_ref().ok_or(PageMapError::MetadataAllocFailed)
    }

    pub(super) fn drop_l2_mapping(&mut self) {
        if self.table.load(Ordering::Acquire).is_null() {
            return;
        }

        self.table.store(core::ptr::null_mut(), Ordering::Release);
        let _ = self.mapping.get_mut().take();
    }

    pub(super) fn assign_segment(
        &self,
        segment: L2Segment,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        self.install_l2()?.assign_segment(segment, value)
    }

    pub(super) fn clear_segment(
        &self,
        segment: L2Segment,
        expected: MapEntry,
    ) -> Result<(), PageMapError> {
        self.l2_table_ref()
            .ok_or(PageMapError::UnexpectedEntry)?
            .clear_segment(segment, expected)
    }

    fn page_entry(&self, index: L2Index) -> Option<MapEntry> {
        self.l2_table_ref()?.get(index)
    }
}

#[repr(C)]
pub(super) struct L2Table {
    pub(super) pages: [AtomicMapEntry; L2_ENTRIES],
}

impl L2Table {
    fn get(&self, index: L2Index) -> Option<MapEntry> {
        let page = self.pages.get(index.get())?.load();
        if page.is_empty() { None } else { Some(page) }
    }

    /// CAS each page `empty → value`. On mid-segment failure, reverse-CAS installed pages.
    pub(super) fn assign_segment(
        &self,
        segment: L2Segment,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        let entries = self
            .pages
            .get(segment.range())
            .ok_or(PageMapError::InvalidRange)?;

        let empty = MapEntry::empty();
        for (written, entry) in entries.iter().enumerate() {
            if entry.compare_exchange(empty, value).is_err() {
                Self::reverse_cas(entries, written, value, empty);
                return Err(PageMapError::Overlap);
            }
        }

        Ok(())
    }

    /// CAS each page `expected → empty`. On mid-segment failure, reverse-CAS to restore.
    pub(super) fn clear_segment(
        &self,
        segment: L2Segment,
        expected: MapEntry,
    ) -> Result<(), PageMapError> {
        let entries = self
            .pages
            .get(segment.range())
            .ok_or(PageMapError::InvalidRange)?;

        let empty = MapEntry::empty();
        for (cleared, entry) in entries.iter().enumerate() {
            if entry.compare_exchange(expected, empty).is_err() {
                Self::reverse_cas(entries, cleared, empty, expected);
                return Err(PageMapError::UnexpectedEntry);
            }
        }

        Ok(())
    }

    fn reverse_cas(entries: &[AtomicMapEntry], count: usize, from: MapEntry, to: MapEntry) {
        for entry in entries.iter().take(count) {
            if entry.compare_exchange(from, to).is_err() {
                Allocator::abort();
            }
        }
    }
}
