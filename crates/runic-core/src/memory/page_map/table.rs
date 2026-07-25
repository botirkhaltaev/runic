use core::{
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use crate::memory::{Mapping, OsMemory};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError, PageOwner,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment, PageRange},
};

/// L1 root: dense hot L2 tips for lock-free `get`, cold write + Mapping sidebands.
///
/// Profile evidence (PR7):
/// - Dense tip words (`*8`) beat master's fat `L1Entry` (`*32`) on the get walk.
/// - `AtomicBool` on [`L2Table`] rounds each L2 mmap to `0x9000` and regresses large.
/// - AoS `L1Cold { write, mapping }` kept L2 at `0x8000` but lost the small-churn win
///   (callgrind: fatter `dealloc` codegen). Keep tips + Mapping SoA like the churn
///   winner, park write in its own cold array, keep [`L2Table`] at exactly `0x8000`.
///
/// # Zero-fill
///
/// Anonymous mmap yields null tips, unlocked writes, `mappings` niche `None`.
#[repr(C)]
pub(super) struct L1Table {
    /// Hot get path only. Indexed by [`L1Index`]; null ⇒ no L2 installed.
    tables: [AtomicPtr<L2Table>; L1_ENTRIES],
    /// Per-L2 stamp exclusion. Not read by `get`.
    writes: [AtomicBool; L1_ENTRIES],
    /// L2 mmap ownership. Written by install CAS winner; read only on `PageMap` drop.
    mappings: [UnsafeCell<Option<Mapping>>; L1_ENTRIES],
}

/// Exclusive stamp access to every distinct L2 touched by `range`.
///
/// Locks in ascending L1 order on construction; unlocks on drop so insert/remove
/// cannot forget an unlock across early returns.
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
    /// Lock-free owner lookup. Touches only `tables` + the L2 page slot — never cold arrays.
    #[inline]
    pub(super) fn owner(&self, l1_index: L1Index, l2_index: L2Index) -> Option<PageOwner> {
        let l2 = self.l2_table_ref(l1_index)?;
        l2.owner(l2_index)
    }

    #[inline]
    pub(super) fn l2_table_ref(&self, index: L1Index) -> Option<&L2Table> {
        let table = NonNull::new(self.table_slot(index).load(Ordering::Acquire))?;

        // SAFETY: `table` is the live L2 pointer published for this L1 index for the PageMap lifetime.
        Some(unsafe { table.as_ref() })
    }

    pub(super) fn install_l2(&self, index: L1Index) -> Result<&L2Table, PageMapError> {
        if let Some(table) = self.l2_table_ref(index) {
            return Ok(table);
        }

        let mapping =
            OsMemory::map(size_of::<L2Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
        let ptr = mapping.base().cast::<L2Table>().as_ptr();
        let slot = self.table_slot(index);

        match slot.compare_exchange(
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

    /// Lock distinct L2 write flags in ascending L1 order (segment iteration order).
    ///
    /// Caller must have installed L2s for every touched index (insert) or accept that a
    /// missing L2 is an invariant violation (remove after a published range).
    pub(super) fn lock_range(&self, range: PageRange) -> L1WriteGuard<'_> {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            self.lock_write(segment.l1);
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
            self.unlock_write(segment.l1);
            prev = Some(segment.l1);
        }
    }

    /// Validate empty then store. Caller must hold write locks for every touched L2.
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

    /// Validate expected then clear. Caller must hold write locks for every touched L2.
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
            if table.get_mut().is_null() {
                continue;
            }
            *table.get_mut() = core::ptr::null_mut();
            let _ = mapping.get_mut().take();
        }
    }

    fn lock_write(&self, index: L1Index) {
        let write = self.write_slot(index);
        while write
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn unlock_write(&self, index: L1Index) {
        self.write_slot(index).store(false, Ordering::Release);
    }

    #[inline]
    fn table_slot(&self, index: L1Index) -> &AtomicPtr<L2Table> {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.tables.get_unchecked(index.get()) }
    }

    #[inline]
    fn write_slot(&self, index: L1Index) -> &AtomicBool {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.writes.get_unchecked(index.get()) }
    }

    #[inline]
    fn mapping_slot(&self, index: L1Index) -> &UnsafeCell<Option<Mapping>> {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.mappings.get_unchecked(index.get()) }
    }
}

// SAFETY: `tables` are published atomically for lock-free get. `writes` serialize stamp
// mutation per L2. `mappings` are written once by the install CAS winner and read only on
// exclusive `PageMap` drop — `get` never touches cold arrays. Zero-filled mmap is valid.
unsafe impl Sync for L1Table {}

/// Per-page stamps. Exactly eight pages (`0x8000`).
///
/// # Zero-fill
///
/// Anonymous mmap yields empty `pages`.
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

#[cfg(test)]
mod zero_fill_tests {
    use super::*;

    #[test]
    fn l2_table_is_exact_eight_pages() {
        assert_eq!(size_of::<L2Table>(), 0x8000);
    }

    #[test]
    fn l1_write_slot_zeroed_is_unlocked() {
        // SAFETY: proves mmap zero-fill on cold write flags.
        let write: AtomicBool = unsafe { core::mem::zeroed() };
        assert!(!write.load(Ordering::Relaxed));
    }

    #[test]
    fn l1_mapping_slot_zeroed_is_none() {
        // SAFETY: proves `Option<Mapping>` all-zero niche used by L1 sideband mmap.
        let mapping: Option<Mapping> = unsafe { core::mem::zeroed() };
        assert!(mapping.is_none());
    }
}
