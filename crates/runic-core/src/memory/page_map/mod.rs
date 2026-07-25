use core::{
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use spin::Mutex;

use crate::{
    arena::Arena,
    heap::{Extent, Run},
    memory::{Mapping, OsMemory, PAGE_SIZE},
};

mod entry;
mod page;
mod table;

#[cfg(test)]
mod tests;

use entry::MapEntry;
use page::{L1Index, Page, PageRange};
use table::L1Table;

const PAGE_SHIFT: usize = 12;
const L2_BITS: usize = 12;
const L2_ENTRIES: usize = 1 << L2_BITS;
const L1_ENTRIES: usize = 1 << (48 - PAGE_SHIFT - L2_BITS);
const ADDRESSABLE_PAGES: usize = L1_ENTRIES * L2_ENTRIES;

/// Sparse L2 mmap registry capacity (install/drop only; not on the get path).
const MAX_L2_MAPPINGS: u32 = 65_536;

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
    /// Once installed, retained until drop. Written only by the CAS winner of [`Self::l1_or_init`];
    /// read only under exclusive `Drop`.
    l1_mapping: UnsafeCell<Option<Mapping>>,
    /// L2 mmap ownership for CAS winners. Touched only on install and `Drop` — never by `get`.
    l2_mappings: Mutex<Arena<Mapping>>,
}

// SAFETY: `l1` is published atomically for lock-free get. `l1_mapping` is written once by the
// install CAS winner and read only on exclusive drop. `l2_mappings` is guarded by its mutex and
// is never accessed by `get`.
unsafe impl Sync for PageMap {}

impl PageMap {
    pub(crate) fn new() -> Self {
        Self {
            l1: AtomicPtr::new(core::ptr::null_mut()),
            l1_mapping: UnsafeCell::new(None),
            l2_mappings: Mutex::new(Arena::new(MAX_L2_MAPPINGS)),
        }
    }

    /// Lock-free ownership lookup: L1 tip → dense slot tip → page entry → [`PageOwner`].
    #[inline]
    pub(crate) fn get(&self, ptr: NonNull<u8>) -> Option<PageOwner> {
        let (l1_index, l2_index) = Page::containing(ptr).indexes()?;
        self.l1()?.owner(l1_index, l2_index)
    }

    pub(crate) fn publish_run(
        &self,
        mapping: &Mapping,
        run: NonNull<Run>,
    ) -> Result<(), PageMapError> {
        let range = PageRange::from_mapping(mapping).ok_or(PageMapError::InvalidRange)?;
        self.insert(range, PageOwner::Run(run))
    }

    pub(crate) fn publish_extent(
        &self,
        mapping: &Mapping,
        extent: NonNull<Extent>,
    ) -> Result<(), PageMapError> {
        let range = PageRange::from_mapping(mapping).ok_or(PageMapError::InvalidRange)?;
        self.insert(range, PageOwner::Extent(extent))
    }

    pub(crate) fn unpublish_extent(
        &self,
        mapping: &Mapping,
        extent: NonNull<Extent>,
    ) -> Result<(), PageMapError> {
        let range = PageRange::from_mapping(mapping).ok_or(PageMapError::InvalidRange)?;
        self.remove(range, PageOwner::Extent(extent))
    }

    fn insert(&self, range: PageRange, entry: PageOwner) -> Result<(), PageMapError> {
        let value = MapEntry::from_owner(entry).ok_or(PageMapError::InvalidRange)?;
        let l1 = self.l1_or_init()?;

        for segment in range.segments() {
            self.install_l2(l1, segment.l1)?;
        }

        let _guard = l1.lock_range(range);
        l1.stamp_insert(range, value)
    }

    fn remove(&self, range: PageRange, expected: PageOwner) -> Result<(), PageMapError> {
        let expected = MapEntry::from_owner(expected).ok_or(PageMapError::InvalidRange)?;
        let l1 = self.l1().ok_or(PageMapError::UnexpectedEntry)?;

        let _guard = l1.lock_range(range);
        l1.stamp_remove(range, expected)
    }

    fn install_l2(&self, l1: &L1Table, index: L1Index) -> Result<(), PageMapError> {
        if l1.l2_table_ref(index).is_some() {
            return Ok(());
        }

        let mapping =
            OsMemory::map(size_of::<table::L2Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
        let ptr = mapping.base().cast::<table::L2Table>().as_ptr();
        let claim = {
            let mut arena = self.l2_mappings.lock();
            arena.claim().ok_or(PageMapError::MetadataAllocFailed)?
        };

        if l1.cas_tip(index, ptr).is_ok() {
            let mut arena = self.l2_mappings.lock();
            arena
                .insert(claim, mapping)
                .ok_or(PageMapError::MetadataAllocFailed)?;
            Ok(())
        } else {
            self.l2_mappings.lock().release(claim);
            drop(mapping);
            Ok(())
        }
    }

    #[inline]
    fn l1(&self) -> Option<&L1Table> {
        let l1 = NonNull::new(self.l1.load(Ordering::Acquire))?;

        // SAFETY: `l1` points at the anonymous mmap owned by `l1_mapping` until PageMap drop.
        // Zero-filled mmap is a valid empty `L1Table` before any L2 install.
        Some(unsafe { l1.as_ref() })
    }

    fn l1_or_init(&self) -> Result<&L1Table, PageMapError> {
        if let Some(l1) = self.l1() {
            return Ok(l1);
        }

        let mapping =
            OsMemory::map(size_of::<L1Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
        let ptr = mapping.base().cast::<L1Table>().as_ptr();

        match self.l1.compare_exchange(
            core::ptr::null_mut(),
            ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: this thread won the null→ptr CAS; sole writer of `l1_mapping`.
                unsafe {
                    *self.l1_mapping.get() = Some(mapping);
                }
            }
            Err(_) => {
                drop(mapping);
            }
        }

        self.l1().ok_or(PageMapError::MetadataAllocFailed)
    }
}

impl Drop for PageMap {
    fn drop(&mut self) {
        let Some(mut l1_ptr) = NonNull::new(*self.l1.get_mut()) else {
            *self.l2_mappings.get_mut() = Arena::new(0);
            return;
        };
        *self.l1.get_mut() = core::ptr::null_mut();

        // SAFETY: PageMap drop has unique access to the L1 table.
        let l1 = unsafe { l1_ptr.as_mut() };
        l1.clear_tips();

        // Drop L2 mappings (munmap) after tips are cleared.
        *self.l2_mappings.get_mut() = Arena::new(0);
        let _ = self.l1_mapping.get_mut().take();
    }
}

const _: () = assert!(
    PAGE_SIZE == 1 << PAGE_SHIFT,
    "PAGE_SHIFT must match PAGE_SIZE"
);
