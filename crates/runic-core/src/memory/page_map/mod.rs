use core::{
    cell::UnsafeCell,
    mem::{MaybeUninit, size_of},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use spin::Mutex;

use crate::{
    heap::{Extent, Run},
    memory::{AddressRange, Mapping, OsMemory, PAGE_SIZE},
};

mod entry;
mod page;
mod span;
mod table;

#[cfg(test)]
mod tests;

use entry::MapEntry;
use page::{Page, PageRange};
use table::L1Table;

const PAGE_SHIFT: usize = 12;
const L2_BITS: usize = 12;
const L2_ENTRIES: usize = 1 << L2_BITS;
const L1_ENTRIES: usize = 1 << (48 - PAGE_SHIFT - L2_BITS);
const ADDRESSABLE_PAGES: usize = L1_ENTRIES * L2_ENTRIES;
const SPAN_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageMapError {
    InvalidRange,
    MetadataAllocFailed,
    Overlap,
    UnexpectedEntry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageOwner {
    // Pointers must refer to live arena entries until their page-map range is removed.
    Run(NonNull<Run>),
    // Pointers must refer to live arena entries until their page-map range is removed.
    Extent(NonNull<Extent>),
}

pub(crate) struct PageMap {
    l1: AtomicPtr<L1Table>,
    mapping: UnsafeCell<MaybeUninit<Mapping>>,
    writer: Mutex<()>,
}

// SAFETY: The L1 pointer is published atomically. Publication/removal is serialized by
// writer; lookups use atomic reads and L2 tables are retained until PageMap drop.
unsafe impl Sync for PageMap {}

impl PageMap {
    pub(crate) const fn new() -> Self {
        Self {
            l1: AtomicPtr::new(core::ptr::null_mut()),
            mapping: UnsafeCell::new(MaybeUninit::uninit()),
            writer: Mutex::new(()),
        }
    }

    pub(crate) fn get(&self, ptr: NonNull<u8>) -> Option<PageOwner> {
        let (l1_index, l2_index) = Page::containing(ptr).indexes()?;

        self.l1()?.page_entry(l1_index, l2_index)?.owner()
    }

    pub(crate) fn publish_run(
        &self,
        range: AddressRange,
        run: NonNull<Run>,
    ) -> Result<(), PageMapError> {
        let _writer = self.writer.lock();
        let range = PageRange::new(range.base(), range.len()).ok_or(PageMapError::InvalidRange)?;
        self.insert(range, PageOwner::Run(run))
    }

    pub(crate) fn publish_extent(
        &self,
        range: AddressRange,
        extent: NonNull<Extent>,
    ) -> Result<(), PageMapError> {
        let _writer = self.writer.lock();
        let range = PageRange::new(range.base(), range.len()).ok_or(PageMapError::InvalidRange)?;
        self.insert(range, PageOwner::Extent(extent))
    }

    pub(crate) fn unpublish_extent(
        &self,
        range: AddressRange,
        extent: NonNull<Extent>,
    ) -> Result<(), PageMapError> {
        let _writer = self.writer.lock();
        let range = PageRange::new(range.base(), range.len()).ok_or(PageMapError::InvalidRange)?;
        self.remove(range, PageOwner::Extent(extent))
    }

    fn insert(&self, range: PageRange, entry: PageOwner) -> Result<(), PageMapError> {
        let occupied = MapEntry::from_owner(entry).ok_or(PageMapError::InvalidRange)?;

        self.validate_insert(range)?;
        self.prepare_insert(range)?;

        let result = if let Some(l1) = self.l1_mut() {
            let mut result = Ok(());

            for segment in range.segments() {
                let published = match entry {
                    PageOwner::Run(_) => l1.entry(segment.l1)?.assign_direct(segment.l2, occupied),
                    PageOwner::Extent(_) => l1.entry(segment.l1)?.assign_span(segment.l2, occupied),
                };

                if let Err(error) = published {
                    result = Err(error);
                    break;
                }
            }

            result
        } else {
            Err(PageMapError::MetadataAllocFailed)
        };

        if let Err(error) = result {
            self.rollback_insert(range, occupied);

            return Err(error);
        }

        Ok(())
    }

    fn remove(&self, range: PageRange, expected: PageOwner) -> Result<(), PageMapError> {
        self.validate_remove(range, expected)?;

        let l1 = self.l1_mut().ok_or(PageMapError::UnexpectedEntry)?;
        for segment in range.segments() {
            l1.entry(segment.l1)?.clear_segment(segment.l2)?;
        }

        Ok(())
    }

    fn rollback_insert(&self, range: PageRange, entry: MapEntry) {
        let Some(l1) = self.l1_mut() else {
            return;
        };

        for segment in range.segments() {
            if l1
                .entry(segment.l1)
                .and_then(|entry_slot| entry_slot.owns_segment(segment.l2, entry))
                != Ok(true)
            {
                continue;
            }

            let _ = l1
                .entry(segment.l1)
                .and_then(|entry_slot| entry_slot.clear_segment(segment.l2));
        }
    }

    fn l1(&self) -> Option<&L1Table> {
        let l1 = NonNull::new(self.l1.load(Ordering::Acquire))?;

        // SAFETY: l1 points to an mmap allocation sized for L1Table and lives for the process.
        Some(unsafe { l1.as_ref() })
    }

    fn l1_mut(&self) -> Option<&L1Table> {
        self.l1()
    }

    fn l1_or_init(&self) -> Result<&L1Table, PageMapError> {
        if self.l1.load(Ordering::Acquire).is_null() {
            let mapping =
                OsMemory::map(size_of::<L1Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
            let ptr = mapping.base().cast::<L1Table>().as_ptr();
            // SAFETY: insert/remove are externally serialized, and readers cannot observe this
            // mapping until l1 is published below.
            unsafe { (*self.mapping.get()).write(mapping) };
            self.l1.store(ptr, Ordering::Release);
        }

        self.l1_mut().ok_or(PageMapError::MetadataAllocFailed)
    }

    fn validate_insert(&self, range: PageRange) -> Result<(), PageMapError> {
        let Some(l1) = self.l1() else {
            return Ok(());
        };

        let empty = MapEntry::empty();
        for segment in range.segments() {
            if !l1.entry(segment.l1)?.owns_segment(segment.l2, empty)? {
                return Err(PageMapError::Overlap);
            }
        }

        Ok(())
    }

    fn validate_remove(&self, range: PageRange, expected: PageOwner) -> Result<(), PageMapError> {
        let expected = MapEntry::from_owner(expected).ok_or(PageMapError::InvalidRange)?;

        let Some(l1) = self.l1() else {
            return Err(PageMapError::UnexpectedEntry);
        };

        for segment in range.segments() {
            if !l1.entry(segment.l1)?.owns_segment(segment.l2, expected)? {
                return Err(PageMapError::UnexpectedEntry);
            }
        }

        Ok(())
    }

    fn prepare_insert(&self, range: PageRange) -> Result<(), PageMapError> {
        let result = {
            let l1 = self.l1_or_init()?;
            let mut result = Ok(());

            for segment in range.segments() {
                if let Err(error) = l1.ensure_l2_table(segment.l1) {
                    result = Err(error);
                    break;
                }
            }

            result
        };

        result?;

        Ok(())
    }
}

impl Drop for PageMap {
    fn drop(&mut self) {
        let Some(mut l1_ptr) = NonNull::new(self.l1.load(Ordering::Acquire)) else {
            return;
        };
        // SAFETY: PageMap drop has unique access to the L1 table.
        let l1 = unsafe { l1_ptr.as_mut() };

        for entry in &mut l1.entries {
            entry.drop_l2_mapping();
        }
        // SAFETY: l1 was published only after mapping initialization.
        unsafe { self.mapping.get_mut().assume_init_drop() };
    }
}

const _: () = assert!(
    PAGE_SIZE == 1 << PAGE_SHIFT,
    "PAGE_SHIFT must match PAGE_SIZE"
);
