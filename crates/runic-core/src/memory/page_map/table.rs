use core::{
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::memory::{Mapping, OsMemory};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError, PageOwner,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment, PageRange},
};

/// Write-exclusion flag stolen from the page-aligned L2 tip pointer (bit 0).
const WRITE_BIT: usize = 1;

/// L1 root: dense hot tips (get) + cold Mapping ownership sideband (install/drop).
///
/// Per-L2 stamp exclusion uses [`WRITE_BIT`] on the tip pointer so [`L2Table`] stays
/// exactly eight pages (`0x8000`). `get` masks the bit off after Acquire load.
///
/// # Zero-fill
///
/// Anonymous mmap yields null tips and `mappings` niche `None`.
#[repr(C)]
pub(super) struct L1Table {
    /// Hot get/stamp tips. Null ⇒ no L2. Bit 0 ⇒ write lock held (only when non-null).
    tables: [AtomicPtr<L2Table>; L1_ENTRIES],
    /// L2 mmap ownership. Written by install CAS winner; read only on `PageMap` drop.
    mappings: [UnsafeCell<Option<Mapping>>; L1_ENTRIES],
}

/// Exclusive stamp access to every distinct L1 tip touched by `range`.
pub(super) struct L1WriteGuard<'a> {
    l1: &'a L1Table,
    range: PageRange,
}

impl Drop for L1WriteGuard<'_> {
    fn drop(&mut self) {
        self.l1.unlock_range(self.range);
    }
}

impl L1Table {
    /// Lock-free owner lookup. Touches only tip words + L2 page slots — never `mappings`.
    #[inline]
    pub(super) fn owner(&self, l1_index: L1Index, l2_index: L2Index) -> Option<PageOwner> {
        let l2 = self.l2_table_ref(l1_index)?;
        l2.owner(l2_index)
    }

    #[inline]
    pub(super) fn l2_table_ref(&self, index: L1Index) -> Option<&L2Table> {
        let raw = self.tip_slot(index).load(Ordering::Acquire);
        let addr = raw.addr() & !WRITE_BIT;
        let table = NonNull::new(core::ptr::with_exposed_provenance_mut(addr))?;

        // SAFETY: published L2 tip (bit 0 masked) lives for the PageMap lifetime.
        Some(unsafe { table.as_ref() })
    }

    pub(super) fn install_l2(&self, index: L1Index) -> Result<&L2Table, PageMapError> {
        if let Some(table) = self.l2_table_ref(index) {
            return Ok(table);
        }

        let mapping =
            OsMemory::map(size_of::<L2Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
        let ptr = mapping.base().cast::<L2Table>().as_ptr();
        debug_assert_eq!(ptr.addr() & WRITE_BIT, 0);

        match self.tip_slot(index).compare_exchange(
            core::ptr::null_mut(),
            ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: this thread won the null→ptr CAS; sole writer of `mappings[index]`.
                unsafe {
                    *self.mapping_slot(index).get() = Some(mapping);
                }
            }
            Err(_) => {
                drop(mapping);
            }
        }

        self.l2_table_ref(index)
            .ok_or(PageMapError::MetadataAllocFailed)
    }

    pub(super) fn lock_range(&self, range: PageRange) -> L1WriteGuard<'_> {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            self.lock_tip(segment.l1);
            prev = Some(segment.l1);
        }
        L1WriteGuard { l1: self, range }
    }

    fn unlock_range(&self, range: PageRange) {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            self.unlock_tip(segment.l1);
            prev = Some(segment.l1);
        }
    }

    fn lock_tip(&self, index: L1Index) {
        let slot = self.tip_slot(index);
        loop {
            let cur = slot.load(Ordering::Relaxed);
            let unlocked_addr = cur.addr() & !WRITE_BIT;
            assert!(
                unlocked_addr != 0,
                "PageMap: L2 must be installed before stamp lock"
            );
            let unlocked = core::ptr::with_exposed_provenance_mut(unlocked_addr);
            let locked = core::ptr::with_exposed_provenance_mut(unlocked_addr | WRITE_BIT);
            if slot
                .compare_exchange(unlocked, locked, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn unlock_tip(&self, index: L1Index) {
        let slot = self.tip_slot(index);
        loop {
            let cur = slot.load(Ordering::Relaxed);
            let unlocked = core::ptr::with_exposed_provenance_mut(cur.addr() & !WRITE_BIT);
            if slot
                .compare_exchange(cur, unlocked, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(super) fn stamp_insert(
        &self,
        range: PageRange,
        value: MapEntry,
    ) -> Result<(), PageMapError> {
        let empty = MapEntry::empty();
        for segment in range.segments() {
            let table = self
                .l2_table_ref(segment.l1)
                .ok_or(PageMapError::MetadataAllocFailed)?;
            if !table.segment_matches(segment.l2, empty)? {
                return Err(PageMapError::Overlap);
            }
        }

        for segment in range.segments() {
            let table = self
                .l2_table_ref(segment.l1)
                .ok_or(PageMapError::MetadataAllocFailed)?;
            table.write_pages(segment.l2, value)?;
        }

        Ok(())
    }

    pub(super) fn stamp_remove(
        &self,
        range: PageRange,
        expected: MapEntry,
    ) -> Result<(), PageMapError> {
        for segment in range.segments() {
            let table = self
                .l2_table_ref(segment.l1)
                .ok_or(PageMapError::UnexpectedEntry)?;
            if !table.segment_matches(segment.l2, expected)? {
                return Err(PageMapError::UnexpectedEntry);
            }
        }

        let empty = MapEntry::empty();
        for segment in range.segments() {
            let table = self
                .l2_table_ref(segment.l1)
                .ok_or(PageMapError::UnexpectedEntry)?;
            table.write_pages(segment.l2, empty)?;
        }

        Ok(())
    }

    pub(super) fn drop_l2_mappings(&mut self) {
        for (table, mapping) in self.tables.iter_mut().zip(self.mappings.iter_mut()) {
            let raw = table.get_mut().addr() & !WRITE_BIT;
            if raw == 0 {
                continue;
            }
            *table.get_mut() = core::ptr::null_mut();
            let _ = mapping.get_mut().take();
        }
    }

    #[inline]
    fn tip_slot(&self, index: L1Index) -> &AtomicPtr<L2Table> {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.tables.get_unchecked(index.get()) }
    }

    #[inline]
    fn mapping_slot(&self, index: L1Index) -> &UnsafeCell<Option<Mapping>> {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.mappings.get_unchecked(index.get()) }
    }
}

/// Per-page stamps. Exactly eight pages — write exclusion lives in the L1 tip bit.
#[repr(C)]
pub(super) struct L2Table {
    pub(super) pages: [AtomicMapEntry; L2_ENTRIES],
}

const _: () = assert!(size_of::<L2Table>() == 0x8000);

impl L2Table {
    #[inline]
    pub(super) fn owner(&self, index: L2Index) -> Option<PageOwner> {
        // SAFETY: `L2Index` is only constructed for values `< L2_ENTRIES`.
        let entry = unsafe { self.pages.get_unchecked(index.get()) };
        entry.load().owner()
    }

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

#[cfg(test)]
mod zero_fill_tests {
    use super::*;

    #[test]
    fn tip_zeroed_is_null_and_l2_is_eight_pages() {
        let tip: AtomicPtr<L2Table> = AtomicPtr::new(core::ptr::null_mut());
        assert!(tip.load(Ordering::Relaxed).is_null());
        assert_eq!(size_of::<L2Table>(), 0x8000);
    }

    #[test]
    fn mapping_slot_zeroed_is_none() {
        // SAFETY: proves `Option<Mapping>` all-zero niche used by L1 sideband mmap.
        let mapping: Option<Mapping> = unsafe { core::mem::zeroed() };
        assert!(mapping.is_none());
    }
}
