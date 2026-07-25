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

/// Two-array L1 root: dense hot L2 pointers for lock-free `get`, cold meta for install/stamp.
///
/// # Zero-fill
///
/// Anonymous mmap yields valid empty state: null `tables`, unlocked `meta.write`,
/// `meta.mapping` niche `None`.
#[repr(C)]
pub(super) struct L1Table {
    /// Hot get path only. Indexed by [`L1Index`]; null ⇒ no L2 installed.
    tables: [AtomicPtr<L2Table>; L1_ENTRIES],
    /// Stamp exclusion + L2 mmap ownership. Indexed in lockstep with `tables`.
    meta: [L1Meta; L1_ENTRIES],
}

/// Exclusive stamp access to every distinct L1 entry touched by `range`.
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
    /// Lock-free owner lookup. Touches only `tables` + the L2 page slot — never `meta`.
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
        let meta = self.meta_slot(index);

        match slot.compare_exchange(
            core::ptr::null_mut(),
            ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: this thread won the null→ptr CAS; sole writer of `meta.mapping`.
                unsafe {
                    *meta.mapping.get() = Some(mapping);
                }
            }
            Err(_) => {
                drop(mapping);
            }
        }

        self.l2_table_ref(index)
            .ok_or(PageMapError::MetadataAllocFailed)
    }

    /// Lock distinct L1 metas in ascending L1 order (segment iteration order).
    pub(super) fn lock_range(&self, range: PageRange) -> L1WriteGuard<'_> {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            self.meta_slot(segment.l1).lock_write();
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
            self.meta_slot(segment.l1).unlock_write();
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

    /// Validate expected then clear. Caller must hold write locks for every touched L1 entry.
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
        for (table, meta) in self.tables.iter_mut().zip(self.meta.iter_mut()) {
            if table.get_mut().is_null() {
                continue;
            }
            *table.get_mut() = core::ptr::null_mut();
            let _ = meta.mapping.get_mut().take();
        }
    }

    #[inline]
    fn table_slot(&self, index: L1Index) -> &AtomicPtr<L2Table> {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.tables.get_unchecked(index.get()) }
    }

    #[inline]
    fn meta_slot(&self, index: L1Index) -> &L1Meta {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.meta.get_unchecked(index.get()) }
    }
}

/// Cold per-L1 state: stamp exclusion and L2 mmap ownership. Not read by `get`.
///
/// # Zero-fill
///
/// `AtomicBool` false (unlocked); `Option<Mapping>` all-zero niche = `None`.
#[repr(C)]
struct L1Meta {
    write: AtomicBool,
    mapping: UnsafeCell<Option<Mapping>>,
}

// SAFETY: `write` serializes stamp mutation for the paired L2. `mapping` is written once by the
// install CAS winner and read only on exclusive `PageMap` drop — `get` never touches `L1Meta`.
unsafe impl Sync for L1Meta {}

impl L1Meta {
    fn lock_write(&self) {
        while self
            .write
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn unlock_write(&self) {
        self.write.store(false, Ordering::Release);
    }
}

#[repr(C)]
pub(super) struct L2Table {
    pub(super) pages: [AtomicMapEntry; L2_ENTRIES],
}

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
    fn l1_meta_zeroed_is_unlocked_none_mapping() {
        // SAFETY: proves mmap zero-fill niches on cold `L1Meta`.
        let meta: L1Meta = unsafe { core::mem::zeroed() };

        assert!(!meta.write.load(Ordering::Relaxed));
        // SAFETY: exclusive local value; no concurrent readers of `mapping`.
        assert!(unsafe { (*meta.mapping.get()).is_none() });
    }

    #[test]
    fn l1_table_slot_zeroed_is_null() {
        let slot: AtomicPtr<L2Table> = AtomicPtr::new(core::ptr::null_mut());
        assert!(slot.load(Ordering::Relaxed).is_null());
    }
}
