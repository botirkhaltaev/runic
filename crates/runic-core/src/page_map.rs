use core::{
    mem::{MaybeUninit, size_of},
    ptr::NonNull,
};

use crate::{
    address::AddressRange,
    extent::ExtentId,
    os_memory::{Mapping, OsMemory, PAGE_SIZE},
    run::RunId,
};

const PAGE_SHIFT: usize = 12;
const L2_BITS: usize = 12;
const L2_ENTRIES: usize = 1 << L2_BITS;
const L1_ENTRIES: usize = 1 << (48 - PAGE_SHIFT - L2_BITS);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageMapError {
    InvalidRange,
    MetadataAllocFailed,
    Overlap,
    UnexpectedEntry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageEntry {
    Run(RunId),
    Extent(ExtentId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmptyL2Tables {
    Release,
    Retain,
}

#[derive(Clone, Copy)]
pub(crate) struct PageRange {
    first: Page,
    last: Page,
}

impl PageRange {
    pub(crate) fn new(base: NonNull<u8>, len: usize) -> Option<Self> {
        let first = Page::containing(base);
        let end_addr = base.as_ptr().addr().checked_add(len.checked_sub(1)?)?;
        let last = Page {
            number: (end_addr >> PAGE_SHIFT).checked_add(1)?,
        };

        Some(Self { first, last })
    }

    pub(crate) fn from_range(range: AddressRange) -> Option<Self> {
        Self::new(range.base(), range.len())
    }

    fn pages(self) -> Pages {
        Pages {
            next: self.first,
            last: self.last,
        }
    }
}

#[derive(Clone, Copy)]
struct Page {
    number: usize,
}

impl Page {
    fn containing(ptr: NonNull<u8>) -> Self {
        Self {
            number: ptr.as_ptr().addr() >> PAGE_SHIFT,
        }
    }

    const fn indexes(self) -> Option<(L1Index, L2Index)> {
        let l2 = self.number & (L2_ENTRIES - 1);
        let l1 = self.number >> L2_BITS;

        if l1 >= L1_ENTRIES {
            return None;
        }

        Some((L1Index { index: l1 }, L2Index { index: l2 }))
    }
}

struct Pages {
    next: Page,
    last: Page,
}

impl Iterator for Pages {
    type Item = Page;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next.number >= self.last.number {
            return None;
        }

        let page = self.next;
        self.next.number = self.next.number.checked_add(1)?;
        Some(page)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct L1Index {
    index: usize,
}

impl L1Index {
    const fn get(self) -> usize {
        self.index
    }
}

#[derive(Clone, Copy)]
struct L2Index {
    index: usize,
}

impl L2Index {
    const fn get(self) -> usize {
        self.index
    }
}

#[repr(C)]
struct L1Table {
    entries: [L1Entry; L1_ENTRIES],
}

impl L1Table {
    fn get(&self, index: L1Index) -> Option<NonNull<L2Table>> {
        self.entries.get(index.get()).and_then(L1Entry::l2_table)
    }

    fn get_or_create(&mut self, index: L1Index) -> Option<NonNull<L2Table>> {
        let entry = self.entries.get_mut(index.get())?;

        if let Some(table) = entry.l2_table() {
            return Some(table);
        }

        let mapping = OsMemory::map(size_of::<L2Table>())?;
        let table = mapping.base().cast::<L2Table>();
        entry.set(mapping);

        Some(table)
    }

    fn clear_empty_l2(&mut self, index: L1Index) -> bool {
        self.entries.get_mut(index.get()).is_some_and(|entry| {
            if !entry.is_empty_l2() {
                return false;
            }

            entry.clear_l2()
        })
    }

    fn set(&mut self, l1_index: L1Index, l2_index: L2Index, value: MapEntry) -> bool {
        let Some(entry) = self.entries.get_mut(l1_index.get()) else {
            return false;
        };
        let Some(mut table) = entry.l2_table() else {
            return false;
        };

        // SAFETY: l2_table returns the live L2 table pointer owned by this L1 entry.
        let table = unsafe { table.as_mut() };
        let Some(previous) = table.get(l2_index) else {
            return false;
        };

        if !table.set(l2_index, value) {
            return false;
        }

        entry.record_transition(previous, value);

        true
    }

    fn clear(&mut self, l1_index: L1Index, l2_index: L2Index) -> bool {
        self.set(l1_index, l2_index, MapEntry::empty())
    }
}

#[repr(C)]
struct L1Entry {
    mapping: MaybeUninit<Mapping>,
    state: L1EntryState,
    occupied_pages: u32,
}

impl L1Entry {
    fn l2_table(&self) -> Option<NonNull<L2Table>> {
        self.mapping()
            .map(|mapping| mapping.base().cast::<L2Table>())
    }

    fn mapping(&self) -> Option<&Mapping> {
        if !self.state.is_occupied() {
            return None;
        }

        // SAFETY: occupied state is set only after mapping.write initializes the slot.
        Some(unsafe { self.mapping.assume_init_ref() })
    }

    fn set(&mut self, mapping: Mapping) {
        self.mapping.write(mapping);
        self.state = L1EntryState::occupied();
        self.occupied_pages = 0;
    }

    fn remove_mapping(&mut self) -> Option<Mapping> {
        if !self.state.is_occupied() {
            return None;
        }

        self.state = L1EntryState::empty();

        // SAFETY: occupied state was true on entry, so the slot contains an initialized Mapping.
        Some(unsafe { self.mapping.assume_init_read() })
    }

    fn is_empty_l2(&self) -> bool {
        self.state.is_occupied() && self.occupied_pages == 0
    }

    fn clear_l2(&mut self) -> bool {
        let removed = self.remove_mapping().is_some();
        if removed {
            self.occupied_pages = 0;
        }

        removed
    }

    fn record_transition(&mut self, previous: MapEntry, next: MapEntry) {
        match (previous.is_empty(), next.is_empty()) {
            (true, false) => {
                self.occupied_pages = self.occupied_pages.saturating_add(1);
            }
            (false, true) => {
                self.occupied_pages = self.occupied_pages.saturating_sub(1);
            }
            _ => {}
        }
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct L1EntryState {
    raw: u8,
}

impl L1EntryState {
    const EMPTY: u8 = 0;
    const OCCUPIED: u8 = 1;

    const fn empty() -> Self {
        Self { raw: Self::EMPTY }
    }

    const fn occupied() -> Self {
        Self {
            raw: Self::OCCUPIED,
        }
    }

    const fn is_occupied(self) -> bool {
        self.raw == Self::OCCUPIED
    }
}

#[repr(C)]
struct L2Table {
    entries: [MapEntry; L2_ENTRIES],
}

impl L2Table {
    fn get(&self, index: L2Index) -> Option<MapEntry> {
        self.entries.get(index.get()).copied()
    }

    fn set(&mut self, index: L2Index, value: MapEntry) -> bool {
        let Some(entry) = self.entries.get_mut(index.get()) else {
            return false;
        };

        *entry = value;
        true
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct MapEntry {
    raw: u32,
}

impl MapEntry {
    const KIND_EXTENT: u32 = 1 << 31;
    const ID_MASK: u32 = !Self::KIND_EXTENT;

    const fn empty() -> Self {
        Self { raw: 0 }
    }

    fn occupied(entry: PageEntry) -> Option<Self> {
        match entry {
            PageEntry::Run(id) => Self::encode(id.index(), 0),
            PageEntry::Extent(id) => Self::encode(id.index(), Self::KIND_EXTENT),
        }
    }

    fn encode(id: u32, kind: u32) -> Option<Self> {
        let encoded = id.checked_add(1)?;

        if encoded > Self::ID_MASK {
            return None;
        }

        Some(Self {
            raw: kind | encoded,
        })
    }

    const fn is_empty(self) -> bool {
        self.raw == 0
    }

    fn page(self) -> Option<PageEntry> {
        if self.is_empty() {
            return None;
        }

        let raw = (self.raw & Self::ID_MASK).checked_sub(1)?;

        if self.raw & Self::KIND_EXTENT == 0 {
            RunId::from_index(raw).map(PageEntry::Run)
        } else {
            ExtentId::from_index(raw).map(PageEntry::Extent)
        }
    }
}

pub(crate) struct PageMap {
    l1: Option<L1Map>,
}

struct L1Map {
    mapping: Mapping,
}

// SAFETY: L1Map owns mmap-backed metadata. Moving ownership to another
// thread does not allow concurrent mutation; global access remains locked.
unsafe impl Send for L1Map {}

impl PageMap {
    pub(crate) const fn new() -> Self {
        Self { l1: None }
    }

    pub(crate) fn get(&self, ptr: NonNull<u8>) -> Option<PageEntry> {
        let (l1_index, l2_index) = Page::containing(ptr).indexes()?;
        let table = self.l1()?.get(l1_index)?;

        // SAFETY: L1Entry only stores non-null L2 table pointers allocated by L1Table::get_or_create.
        let entry = unsafe { table.as_ref() }.get(l2_index)?;
        entry.page()
    }

    pub(crate) fn insert(
        &mut self,
        range: PageRange,
        entry: PageEntry,
    ) -> Result<(), PageMapError> {
        let occupied = MapEntry::occupied(entry).ok_or(PageMapError::InvalidRange)?;

        self.validate_insert(range)?;
        self.prepare_insert(range)?;

        for page in range.pages() {
            if let Err(error) = self.set_page(page, occupied) {
                self.clear_matching(range, occupied);
                self.clear_empty_l2_tables(range);

                return Err(error);
            }
        }

        Ok(())
    }

    pub(crate) fn remove(
        &mut self,
        range: PageRange,
        expected: PageEntry,
        empty_l2_tables: EmptyL2Tables,
    ) -> Result<(), PageMapError> {
        self.validate_remove(range, expected)?;

        for page in range.pages() {
            self.clear_page(page)?;
        }

        if empty_l2_tables == EmptyL2Tables::Release {
            self.clear_empty_l2_tables(range);
        }

        Ok(())
    }

    fn clear_matching(&mut self, range: PageRange, entry: MapEntry) {
        for page in range.pages() {
            if self.entry_for_page(page).ok().flatten() != Some(entry) {
                continue;
            }

            let _ = self.clear_page(page);
        }
    }

    fn clear_empty_l2_tables(&mut self, range: PageRange) {
        let mut previous = None;

        for page in range.pages() {
            let Some((l1_index, _)) = page.indexes() else {
                continue;
            };

            if previous == Some(l1_index) {
                continue;
            }

            previous = Some(l1_index);

            if let Some(l1) = self.l1_mut() {
                let _ = l1.clear_empty_l2(l1_index);
            }
        }
    }

    fn l1(&self) -> Option<&L1Table> {
        let l1 = self.l1.as_ref()?.as_table();

        // SAFETY: l1 points to an mmap allocation sized for L1Table and lives for the process.
        Some(unsafe { l1.as_ref() })
    }

    fn l1_mut(&mut self) -> Option<&mut L1Table> {
        let mut l1 = self.l1.as_mut()?.as_table();

        // SAFETY: PageMap is only accessed behind the global heap lock.
        Some(unsafe { l1.as_mut() })
    }

    fn l1_or_init(&mut self) -> Result<&mut L1Table, PageMapError> {
        if self.l1.is_none() {
            let mapping =
                OsMemory::map(size_of::<L1Table>()).ok_or(PageMapError::MetadataAllocFailed)?;
            self.l1 = Some(L1Map { mapping });
        }

        self.l1_mut().ok_or(PageMapError::MetadataAllocFailed)
    }

    fn validate_insert(&self, range: PageRange) -> Result<(), PageMapError> {
        for page in range.pages() {
            let existing = self.entry_for_page(page)?;

            if existing.is_some_and(|existing| !existing.is_empty()) {
                return Err(PageMapError::Overlap);
            }
        }

        Ok(())
    }

    fn validate_remove(&self, range: PageRange, expected: PageEntry) -> Result<(), PageMapError> {
        let expected = MapEntry::occupied(expected).ok_or(PageMapError::InvalidRange)?;

        for page in range.pages() {
            if self.entry_for_page(page)? != Some(expected) {
                return Err(PageMapError::UnexpectedEntry);
            }
        }

        Ok(())
    }

    fn entry_for_page(&self, page: Page) -> Result<Option<MapEntry>, PageMapError> {
        let (l1_index, l2_index) = page.indexes().ok_or(PageMapError::InvalidRange)?;
        let Some(table) = self.l1().and_then(|l1| l1.get(l1_index)) else {
            return Ok(None);
        };

        // SAFETY: L1Entry only stores non-null L2 table pointers allocated by L1Table::get_or_create.
        unsafe { table.as_ref() }
            .get(l2_index)
            .map(Some)
            .ok_or(PageMapError::InvalidRange)
    }

    fn prepare_insert(&mut self, range: PageRange) -> Result<(), PageMapError> {
        for page in range.pages() {
            let (l1_index, _) = page.indexes().ok_or(PageMapError::InvalidRange)?;
            if self.l1_or_init()?.get_or_create(l1_index).is_none() {
                self.clear_empty_l2_tables(range);
                return Err(PageMapError::MetadataAllocFailed);
            }
        }

        Ok(())
    }

    fn set_page(&mut self, page: Page, entry: MapEntry) -> Result<(), PageMapError> {
        let (l1_index, l2_index) = page.indexes().ok_or(PageMapError::InvalidRange)?;
        let Some(l1) = self.l1_mut() else {
            return Err(PageMapError::MetadataAllocFailed);
        };

        if l1.set(l1_index, l2_index, entry) {
            Ok(())
        } else {
            Err(PageMapError::InvalidRange)
        }
    }

    fn clear_page(&mut self, page: Page) -> Result<(), PageMapError> {
        let (l1_index, l2_index) = page.indexes().ok_or(PageMapError::InvalidRange)?;
        let Some(l1) = self.l1_mut() else {
            return Err(PageMapError::UnexpectedEntry);
        };

        if l1.clear(l1_index, l2_index) {
            Ok(())
        } else {
            Err(PageMapError::InvalidRange)
        }
    }
}

impl L1Map {
    fn as_table(&self) -> NonNull<L1Table> {
        self.mapping.base().cast::<L1Table>()
    }
}

impl Drop for PageMap {
    fn drop(&mut self) {
        let Some(l1) = self.l1_mut() else {
            return;
        };

        for entry in &mut l1.entries {
            let _ = entry.clear_l2();
        }
    }
}

const _: () = assert!(
    PAGE_SIZE == 1 << PAGE_SHIFT,
    "PAGE_SHIFT must match PAGE_SIZE"
);

#[cfg(test)]
mod tests {
    use super::*;
    fn id(raw: u32) -> RunId {
        RunId::from_index(raw).unwrap()
    }

    fn run(raw: u32) -> PageEntry {
        PageEntry::Run(id(raw))
    }

    fn extent(raw: u32) -> PageEntry {
        PageEntry::Extent(ExtentId::from_index(raw).unwrap())
    }

    fn has_l2_table(map: &PageMap, ptr: NonNull<u8>) -> bool {
        let Some((l1_index, _)) = Page::containing(ptr).indexes() else {
            return false;
        };

        map.l1().and_then(|l1| l1.get(l1_index)).is_some()
    }

    struct TestMapping {
        mapping: crate::os_memory::Mapping,
    }

    impl TestMapping {
        fn new(len: usize) -> Self {
            Self {
                mapping: OsMemory::map(len).unwrap(),
            }
        }

        fn base(&self) -> NonNull<u8> {
            self.mapping.base()
        }

        fn len(&self) -> usize {
            self.mapping.range().len()
        }

        fn page_range(&self) -> PageRange {
            PageRange::new(self.base(), self.len()).unwrap()
        }

        fn ptr_at(&self, offset: usize) -> NonNull<u8> {
            assert!(offset < self.len());

            // SAFETY: offset is asserted in-bounds for this test mapping.
            let raw = unsafe { self.base().as_ptr().add(offset) };
            // SAFETY: raw is derived from a non-null mapping base plus an in-bounds offset.
            unsafe { NonNull::new_unchecked(raw) }
        }
    }

    #[test]
    fn page_map_new_lookup_returns_none() {
        let map = PageMap::new();
        let ptr = NonNull::dangling();

        assert!(map.get(ptr).is_none());
    }

    #[test]
    fn page_map_insert_range_maps_interior_pointer() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(7)).is_ok());

        let interior = mapping.ptr_at(PAGE_SIZE + 17);
        assert_eq!(map.get(interior), Some(run(7)));
    }

    #[test]
    fn page_map_insert_range_maps_extent_entry() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, extent(4)).is_ok());

        let interior = mapping.ptr_at(PAGE_SIZE + 17);
        assert_eq!(map.get(interior), Some(extent(4)));
    }

    #[test]
    fn page_map_remove_range_clears_mapped_pages() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(8)).is_ok());
        assert_eq!(map.remove(range, run(8), EmptyL2Tables::Release), Ok(()));

        assert!(map.get(mapping.base()).is_none());
        let second = mapping.ptr_at(PAGE_SIZE);
        assert!(map.get(second).is_none());
    }

    #[test]
    fn page_map_remove_range_clears_empty_l2_table() {
        let mapping = TestMapping::new(PAGE_SIZE);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(1)).is_ok());
        assert!(has_l2_table(&map, mapping.base()));

        assert_eq!(map.remove(range, run(1), EmptyL2Tables::Release), Ok(()));

        assert!(map.get(mapping.base()).is_none());
        assert!(!has_l2_table(&map, mapping.base()));
    }

    #[test]
    fn page_map_remove_range_keeps_non_empty_l2_table() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let first = mapping.base();
        let second = mapping.ptr_at(PAGE_SIZE);

        assert!(
            map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(second, PAGE_SIZE).unwrap(), run(2))
                .is_ok()
        );

        assert_eq!(
            map.remove(
                PageRange::new(first, PAGE_SIZE).unwrap(),
                run(1),
                EmptyL2Tables::Release,
            ),
            Ok(())
        );

        assert!(map.get(first).is_none());
        assert_eq!(map.get(second), Some(run(2)));
        assert!(has_l2_table(&map, second));
    }

    #[test]
    fn page_map_remove_range_preserves_neighboring_page() {
        let mapping = TestMapping::new(PAGE_SIZE * 3);
        let mut map = PageMap::new();
        let first = mapping.base();
        let second = mapping.ptr_at(PAGE_SIZE);
        let third = mapping.ptr_at(PAGE_SIZE * 2);

        assert!(
            map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(second, PAGE_SIZE).unwrap(), run(2))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(third, PAGE_SIZE).unwrap(), run(3))
                .is_ok()
        );

        assert_eq!(
            map.remove(
                PageRange::new(second, PAGE_SIZE).unwrap(),
                run(2),
                EmptyL2Tables::Release,
            ),
            Ok(())
        );

        assert_eq!(map.get(first), Some(run(1)));
        assert!(map.get(second).is_none());
        assert_eq!(map.get(third), Some(run(3)));
    }

    #[test]
    fn page_map_remove_range_rejects_wrong_owner_without_clearing() {
        let mapping = TestMapping::new(PAGE_SIZE);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(1)).is_ok());

        assert_eq!(
            map.remove(range, run(2), EmptyL2Tables::Release),
            Err(PageMapError::UnexpectedEntry)
        );
        assert_eq!(map.get(mapping.base()), Some(run(1)));
    }

    #[test]
    fn page_map_remove_range_rejects_missing_entry_without_clearing() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let first = mapping.base();
        let second = mapping.ptr_at(PAGE_SIZE);

        assert!(
            map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );

        assert_eq!(
            map.remove(mapping.page_range(), run(1), EmptyL2Tables::Release),
            Err(PageMapError::UnexpectedEntry)
        );
        assert_eq!(map.get(first), Some(run(1)));
        assert!(map.get(second).is_none());
    }

    #[test]
    fn page_map_remove_range_rejects_partial_mismatch_without_clearing() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let first = mapping.base();
        let second = mapping.ptr_at(PAGE_SIZE);

        assert!(
            map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(second, PAGE_SIZE).unwrap(), run(2))
                .is_ok()
        );

        assert_eq!(
            map.remove(mapping.page_range(), run(1), EmptyL2Tables::Release),
            Err(PageMapError::UnexpectedEntry)
        );
        assert_eq!(map.get(first), Some(run(1)));
        assert_eq!(map.get(second), Some(run(2)));
    }

    #[test]
    fn page_map_clear_matching_preserves_other_owners() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let first = mapping.base();
        let second = mapping.ptr_at(PAGE_SIZE);

        assert!(
            map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(second, PAGE_SIZE).unwrap(), run(2))
                .is_ok()
        );

        map.clear_matching(mapping.page_range(), MapEntry::occupied(run(1)).unwrap());

        assert!(map.get(first).is_none());
        assert_eq!(map.get(second), Some(run(2)));
    }

    #[test]
    fn page_map_insert_range_rejects_overlapping_different_run() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let second = mapping.ptr_at(PAGE_SIZE);

        assert!(
            map.insert(
                PageRange::new(mapping.base(), PAGE_SIZE * 2).unwrap(),
                run(11),
            )
            .is_ok()
        );
        assert_eq!(
            map.insert(PageRange::new(second, PAGE_SIZE).unwrap(), run(12)),
            Err(PageMapError::Overlap)
        );
        assert_eq!(map.get(second), Some(run(11)));
    }

    #[test]
    fn page_map_insert_range_rejects_existing_same_entry() {
        let mapping = TestMapping::new(PAGE_SIZE);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(13)).is_ok());
        assert_eq!(map.insert(range, run(13)), Err(PageMapError::Overlap));
        assert_eq!(map.get(mapping.base()), Some(run(13)));
    }

    #[test]
    fn page_map_overlap_validation_does_not_allocate_empty_l2_tables() {
        let mapping = TestMapping::new((L2_ENTRIES * 2 + 2) * PAGE_SIZE);
        let mut map = PageMap::new();
        let (_, base_l2) = Page::containing(mapping.base()).indexes().unwrap();
        let pages_to_next_l2 = L2_ENTRIES - base_l2.get();
        let overlap = mapping.ptr_at(pages_to_next_l2 * PAGE_SIZE);

        assert!(
            map.insert(PageRange::new(overlap, PAGE_SIZE).unwrap(), run(21))
                .is_ok()
        );
        assert!(!has_l2_table(&map, mapping.base()));

        assert_eq!(
            map.insert(
                PageRange::new(mapping.base(), (pages_to_next_l2 + 1) * PAGE_SIZE).unwrap(),
                run(22),
            ),
            Err(PageMapError::Overlap)
        );

        assert!(!has_l2_table(&map, mapping.base()));
        assert_eq!(map.get(mapping.base()), None);
        assert_eq!(map.get(overlap), Some(run(21)));
    }

    #[test]
    fn page_map_insert_range_rejects_zero_len() {
        let mapping = TestMapping::new(PAGE_SIZE);

        assert!(PageRange::new(mapping.base(), 0).is_none());
    }

    #[test]
    fn page_map_insert_range_crosses_l2_boundary() {
        let len = (L2_ENTRIES + 2) * PAGE_SIZE;
        let mapping = TestMapping::new(len);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(10)).is_ok());

        let last = mapping.ptr_at(mapping.len() - 1);
        assert_eq!(map.get(mapping.base()), Some(run(10)));
        assert_eq!(map.get(last), Some(run(10)));
    }
}
