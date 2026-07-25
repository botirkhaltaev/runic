use core::{
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use crate::memory::{Mapping, OsMemory};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment, PageRange},
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

    /// Lock distinct L1 entries in ascending L1 order (segment iteration order).
    pub(super) fn lock_range(&self, range: PageRange) {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            if let Ok(entry) = self.entry(segment.l1) {
                entry.lock_write();
            }
            prev = Some(segment.l1);
        }
    }

    pub(super) fn unlock_range(&self, range: PageRange) {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            if let Ok(entry) = self.entry(segment.l1) {
                entry.unlock_write();
            }
            prev = Some(segment.l1);
        }
    }

    /// Validate empty then store. Caller must hold write locks for every touched L1 entry.
    pub(super) fn stamp_insert(
        &self,
        range: PageRange,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        let empty = MapEntry::empty();
        for segment in range.segments() {
            let table = self
                .entry(segment.l1)?
                .l2_table_ref()
                .ok_or(PageMapError::MetadataAllocFailed)?;
            if !table.segment_matches(segment.l2, empty)? {
                return Err(PageMapError::Overlap);
            }
        }

        for segment in range.segments() {
            let table = self
                .entry(segment.l1)?
                .l2_table_ref()
                .ok_or(PageMapError::MetadataAllocFailed)?;
            table.write_pages(segment.l2, value)?;
        }

        Ok(())
    }

    /// Validate expected then clear. Caller must hold write locks for every touched L1 entry.
    pub(super) fn stamp_remove(
        &self,
        range: PageRange,
        expected: MapEntry,
    ) -> Result<(), PageMapError> {
        for segment in range.segments() {
            let table = self
                .entry(segment.l1)?
                .l2_table_ref()
                .ok_or(PageMapError::UnexpectedEntry)?;
            if !table.segment_matches(segment.l2, expected)? {
                return Err(PageMapError::UnexpectedEntry);
            }
        }

        let empty = MapEntry::empty();
        for segment in range.segments() {
            let table = self
                .entry(segment.l1)?
                .l2_table_ref()
                .ok_or(PageMapError::UnexpectedEntry)?;
            table.write_pages(segment.l2, empty)?;
        }

        Ok(())
    }
}

#[repr(C)]
pub(super) struct L1Entry {
    table: AtomicPtr<L2Table>,
    /// Per-L2 stamp exclusion. Zero-filled mmap ⇒ unlocked (`false`).
    write: AtomicBool,
    /// Once installed, retained until `PageMap` drop. Written only by the CAS winner of
    /// [`Self::install_l2`]; read only under exclusive `PageMap::Drop`.
    mapping: UnsafeCell<Option<Mapping>>,
}

// SAFETY: `table` is published atomically for lock-free get. `write` serializes stamp mutation
// for this L2. `mapping` is written once by the install CAS winner and read only on exclusive
// `PageMap` drop — `get` never touches it.
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

    pub(super) fn lock_write(&self) {
        while self
            .write
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    pub(super) fn unlock_write(&self) {
        self.write.store(false, Ordering::Release);
    }

    pub(super) fn drop_l2_mapping(&mut self) {
        if self.table.load(Ordering::Acquire).is_null() {
            return;
        }

        self.table.store(core::ptr::null_mut(), Ordering::Release);
        let _ = self.mapping.get_mut().take();
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

    /// Caller must hold this L2's write exclusion.
    pub(super) fn segment_matches(
        &self,
        segment: L2Segment,
        expected: MapEntry,
    ) -> Result<bool, PageMapError> {
        let pages = self
            .pages
            .get(segment.range())
            .ok_or(PageMapError::InvalidRange)?;

        Ok(pages.iter().all(|entry| entry.load() == expected))
    }

    /// Caller must hold this L2's write exclusion.
    pub(super) fn write_pages(
        &self,
        segment: L2Segment,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
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
