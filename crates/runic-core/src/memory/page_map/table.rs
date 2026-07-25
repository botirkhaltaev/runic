use core::{
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use super::{
    L1_ENTRIES, L2_ENTRIES, PageMapError, PageOwner,
    entry::{AtomicMapEntry, MapEntry},
    page::{L1Index, L2Index, L2Segment, PageRange},
};

/// Hot L1 root: tip + write exclusion only (16-byte stride).
///
/// L2 mmap ownership is registered sparsely on [`super::PageMap`] (install/drop only),
/// so `get` and stamp never first-touch a parallel Mapping sideband.
///
/// # Zero-fill
///
/// Anonymous mmap yields null tips and unlocked `write`.
#[repr(C)]
pub(super) struct L1Table {
    slots: [L1Slot; L1_ENTRIES],
}

/// Hot per-L1 tip + stamp exclusion. 16-byte stride (half the old tip+write+Mapping entry).
#[repr(C)]
pub(super) struct L1Slot {
    table: AtomicPtr<L2Table>,
    /// Per-L2 stamp exclusion. Zero-filled mmap ⇒ unlocked (`false`).
    write: AtomicBool,
}

const _: () = assert!(size_of::<L1Slot>() == 16);

// SAFETY: `table` is published atomically for lock-free get. `write` serializes stamp mutation
// for this L2. Zero-filled mmap is a valid empty slot.
unsafe impl Sync for L1Slot {}

/// Exclusive stamp access to every distinct L1 slot touched by `range`.
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
    /// Lock-free owner lookup. Touches only slot tips + the L2 page slot.
    #[inline]
    pub(super) fn owner(&self, l1_index: L1Index, l2_index: L2Index) -> Option<PageOwner> {
        let l2 = self.l2_table_ref(l1_index)?;
        l2.owner(l2_index)
    }

    #[inline]
    pub(super) fn l2_table_ref(&self, index: L1Index) -> Option<&L2Table> {
        let table = NonNull::new(self.slot(index).table.load(Ordering::Acquire))?;

        // SAFETY: `table` is the live L2 pointer published for this L1 index for the PageMap lifetime.
        Some(unsafe { table.as_ref() })
    }

    /// Once-only null→tip CAS. `Ok(())` means this thread published `ptr`; `Err` means another
    /// tip is already live (caller must drop its unused mapping).
    pub(super) fn cas_tip(&self, index: L1Index, ptr: *mut L2Table) -> Result<(), *mut L2Table> {
        match self.slot(index).table.compare_exchange(
            core::ptr::null_mut(),
            ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(current) => Err(current),
        }
    }

    /// Lock distinct L1 slots in ascending L1 order (segment iteration order).
    pub(super) fn lock_range(&self, range: PageRange) -> L1WriteGuard<'_> {
        let mut prev = None;
        for segment in range.segments() {
            if prev == Some(segment.l1) {
                continue;
            }
            self.slot(segment.l1).lock_write();
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
            self.slot(segment.l1).unlock_write();
            prev = Some(segment.l1);
        }
    }

    /// Validate empty then store. Caller must hold write locks for every touched L1 slot.
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

    /// Validate expected then clear. Caller must hold write locks for every touched L1 slot.
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

    pub(super) fn clear_tips(&mut self) {
        for slot in &mut self.slots {
            *slot.table.get_mut() = core::ptr::null_mut();
        }
    }

    #[inline]
    fn slot(&self, index: L1Index) -> &L1Slot {
        // SAFETY: `L1Index` is only constructed for values `< L1_ENTRIES`.
        unsafe { self.slots.get_unchecked(index.get()) }
    }
}

impl L1Slot {
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

/// Per-page stamps. Sized for an exact 8-page mmap (`L2_ENTRIES * 8 == 0x8000`).
#[repr(C)]
pub(super) struct L2Table {
    pub(super) pages: [AtomicMapEntry; L2_ENTRIES],
}

const _: () = assert!(size_of::<L2Table>() == L2_ENTRIES * size_of::<AtomicMapEntry>());
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
    fn l1_slot_zeroed_is_unlocked_null_table() {
        // SAFETY: proves mmap zero-fill niches on hot `L1Slot`.
        let slot: L1Slot = unsafe { core::mem::zeroed() };

        assert!(slot.table.load(Ordering::Relaxed).is_null());
        assert!(!slot.write.load(Ordering::Relaxed));
    }

    #[test]
    fn l2_table_is_exact_eight_pages() {
        assert_eq!(size_of::<L2Table>(), 0x8000);
    }
}
