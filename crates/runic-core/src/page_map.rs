use core::{
    mem::{MaybeUninit, size_of},
    num::NonZeroU16,
    ops::Range,
    ptr::NonNull,
};

use crate::{
    extent::ExtentId,
    os_memory::{Mapping, OsMemory, PAGE_SIZE},
    run::RunId,
};

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
pub(crate) enum PageEntry {
    Run(RunId),
    Extent(ExtentId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum L2TablePolicy {
    ReleaseEmpty,
    RetainEmpty,
}

#[derive(Clone, Copy)]
pub(crate) struct PageRange {
    first: Page,
    end: Page,
}

impl PageRange {
    pub(crate) fn new(base: NonNull<u8>, len: usize) -> Option<Self> {
        let first = Page::containing(base);
        let end_addr = base.as_ptr().addr().checked_add(len.checked_sub(1)?)?;
        let end = Page {
            number: (end_addr >> PAGE_SHIFT).checked_add(1)?,
        };

        if first.number >= ADDRESSABLE_PAGES || end.number > ADDRESSABLE_PAGES {
            return None;
        }

        Some(Self { first, end })
    }

    fn segments(self) -> PageSegments {
        PageSegments {
            next_page: self.first.number,
            end_page: self.end.number,
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

#[derive(Clone, Copy)]
struct PageSegment {
    l1: L1Index,
    l2: L2Segment,
}

#[derive(Clone, Copy)]
struct L2Segment {
    first: L2Index,
    pages: PageCount,
}

impl L2Segment {
    fn new(first: L2Index, pages: usize) -> Option<Self> {
        let pages = PageCount::new(pages)?;
        let end = first.get().checked_add(pages.get())?;

        if end > L2_ENTRIES {
            return None;
        }

        Some(Self { first, pages })
    }

    fn range(self) -> Range<usize> {
        let start = self.first.get();
        let end = start + self.pages.get();

        start..end
    }

    fn contains(self, index: L2Index) -> bool {
        self.range().contains(&index.get())
    }

    fn pages(self) -> u32 {
        self.pages.get_u32()
    }
}

#[derive(Clone, Copy)]
struct PageCount {
    value: NonZeroU16,
}

impl PageCount {
    fn new(pages: usize) -> Option<Self> {
        let pages = u16::try_from(pages).ok()?;
        NonZeroU16::new(pages).map(|value| Self { value })
    }

    fn get(self) -> usize {
        usize::from(self.value.get())
    }

    fn get_u32(self) -> u32 {
        u32::from(self.value.get())
    }
}

struct PageSegments {
    next_page: usize,
    end_page: usize,
}

impl Iterator for PageSegments {
    type Item = PageSegment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_page >= self.end_page {
            return None;
        }

        let l2 = self.next_page & (L2_ENTRIES - 1);
        let l1 = self.next_page >> L2_BITS;
        if l1 >= L1_ENTRIES {
            return None;
        }

        let remaining = self.end_page - self.next_page;
        let pages = remaining.min(L2_ENTRIES - l2);
        let next_page = self.next_page.checked_add(pages)?;
        let l2 = L2Segment::new(L2Index { index: l2 }, pages)?;
        self.next_page = next_page;

        Some(PageSegment {
            l1: L1Index { index: l1 },
            l2,
        })
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
    fn page_entry(&self, l1_index: L1Index, l2_index: L2Index) -> Option<MapEntry> {
        self.entries.get(l1_index.get())?.page_entry(l2_index)
    }

    fn entry(&self, index: L1Index) -> Result<&L1Entry, PageMapError> {
        self.entries
            .get(index.get())
            .ok_or(PageMapError::InvalidRange)
    }

    fn entry_mut(&mut self, index: L1Index) -> Result<&mut L1Entry, PageMapError> {
        self.entries
            .get_mut(index.get())
            .ok_or(PageMapError::InvalidRange)
    }

    fn ensure_l2_table(&mut self, index: L1Index) -> Result<(), PageMapError> {
        let entry = self.entry_mut(index)?;

        if entry.has_l2_table() {
            return Ok(());
        }

        let Some(mapping) = OsMemory::map(size_of::<L2Table>()) else {
            return Err(PageMapError::MetadataAllocFailed);
        };
        entry.install_l2_mapping(mapping);

        Ok(())
    }

    fn release_empty_l2(&mut self, index: L1Index) -> bool {
        self.entries.get_mut(index.get()).is_some_and(|entry| {
            if !entry.is_empty_l2() {
                return false;
            }

            entry.clear_l2()
        })
    }
}

#[repr(C)]
struct L1Entry {
    mapping: MaybeUninit<Mapping>,
    state: L1EntryState,
    occupied_pages: u32,
}

impl L1Entry {
    fn has_l2_table(&self) -> bool {
        self.state.is_occupied()
    }

    fn l2_table(&self) -> Option<NonNull<L2Table>> {
        self.mapping()
            .map(|mapping| mapping.base().cast::<L2Table>())
    }

    fn l2_table_ref(&self) -> Option<&L2Table> {
        let table = self.l2_table()?;

        // SAFETY: l2_table returns the live L2 table pointer owned by this L1 entry.
        Some(unsafe { table.as_ref() })
    }

    fn l2_table_mut(&mut self) -> Option<&mut L2Table> {
        let mut table = self.l2_table()?;

        // SAFETY: l2_table returns the live L2 table pointer owned by this L1 entry.
        Some(unsafe { table.as_mut() })
    }

    fn mapping(&self) -> Option<&Mapping> {
        if !self.has_l2_table() {
            return None;
        }

        // SAFETY: occupied state is set only after mapping.write initializes the slot.
        Some(unsafe { self.mapping.assume_init_ref() })
    }

    fn install_l2_mapping(&mut self, mapping: Mapping) {
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
        self.has_l2_table() && self.occupied_pages == 0
    }

    fn clear_l2(&mut self) -> bool {
        let removed = self.remove_mapping().is_some();
        if removed {
            self.occupied_pages = 0;
        }

        removed
    }

    fn owns_segment(&self, segment: L2Segment, expected: MapEntry) -> Result<bool, PageMapError> {
        let Some(table) = self.l2_table_ref() else {
            return Ok(expected.is_empty());
        };

        if expected.is_empty() && self.occupied_pages == 0 {
            return Ok(true);
        }

        table.owns_segment(segment, expected)
    }

    fn page_entry(&self, index: L2Index) -> Option<MapEntry> {
        self.l2_table_ref()?.get(index)
    }

    fn assign_direct(&mut self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .checked_add(segment.pages())
            .ok_or(PageMapError::InvalidRange)?;
        let table = self
            .l2_table_mut()
            .ok_or(PageMapError::MetadataAllocFailed)?;

        table.assign_direct(segment, value)?;
        self.occupied_pages = occupied_pages;

        Ok(())
    }

    fn assign_span(&mut self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .checked_add(segment.pages())
            .ok_or(PageMapError::InvalidRange)?;
        let table = self
            .l2_table_mut()
            .ok_or(PageMapError::MetadataAllocFailed)?;

        table.assign_span(segment, value)?;
        self.occupied_pages = occupied_pages;

        Ok(())
    }

    fn clear_segment(&mut self, segment: L2Segment) -> Result<(), PageMapError> {
        let occupied_pages = self
            .occupied_pages
            .checked_sub(segment.pages())
            .ok_or(PageMapError::UnexpectedEntry)?;
        let table = self.l2_table_mut().ok_or(PageMapError::UnexpectedEntry)?;

        table.clear_segment(segment)?;
        self.occupied_pages = occupied_pages;

        Ok(())
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
    pages: [MapEntry; L2_ENTRIES],
    spans: [SpanSlot; SPAN_SLOTS],
}

impl L2Table {
    fn get(&self, index: L2Index) -> Option<MapEntry> {
        let page = self.pages.get(index.get()).copied()?;
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
        if pages.iter().all(|entry| *entry == expected) {
            return Ok(true);
        }

        Ok(self
            .spans
            .iter()
            .any(|slot| slot.matches(segment, expected)))
    }

    fn assign_direct(&mut self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        self.write_pages(segment, value)
    }

    fn assign_span(&mut self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        if self.install_span(segment, value) {
            return Ok(());
        }

        self.write_pages(segment, value)
    }

    fn clear_segment(&mut self, segment: L2Segment) -> Result<(), PageMapError> {
        if self.clear_span(segment) {
            return Ok(());
        }

        self.write_pages(segment, MapEntry::empty())
    }

    fn segment_is_free(&self, segment: L2Segment) -> bool {
        let Some(pages) = self.pages.get(segment.range()) else {
            return false;
        };

        pages.iter().all(|entry| entry.is_empty())
            && self.spans.iter().all(|slot| !slot.overlaps(segment))
    }

    fn install_span(&mut self, segment: L2Segment, entry: MapEntry) -> bool {
        let record = SpanRecord::new(segment, entry);
        let Some(slot) = self.spans.iter_mut().find(|slot| slot.is_empty()) else {
            return false;
        };

        slot.set(record);
        true
    }

    fn clear_span(&mut self, segment: L2Segment) -> bool {
        let Some(slot) = self.spans.iter_mut().find(|slot| slot.covers(segment)) else {
            return false;
        };

        slot.clear();
        true
    }

    fn write_pages(&mut self, segment: L2Segment, value: MapEntry) -> Result<(), PageMapError> {
        let entries = self
            .pages
            .get_mut(segment.range())
            .ok_or(PageMapError::InvalidRange)?;

        entries.fill(value);

        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SpanRecord {
    first: L2Index,
    pages: PageCount,
    entry: MapEntry,
}

impl SpanRecord {
    const fn new(segment: L2Segment, entry: MapEntry) -> Self {
        Self {
            first: segment.first,
            pages: segment.pages,
            entry,
        }
    }

    fn segment(self) -> L2Segment {
        L2Segment {
            first: self.first,
            pages: self.pages,
        }
    }

    fn entry(self) -> MapEntry {
        self.entry
    }

    fn contains(self, index: L2Index) -> bool {
        self.segment().contains(index)
    }

    fn overlaps(self, segment: L2Segment) -> bool {
        let own = self.segment().range();
        let other = segment.range();

        own.start < other.end && other.start < own.end
    }

    fn matches(self, segment: L2Segment, entry: MapEntry) -> bool {
        self.segment().range() == segment.range() && self.entry == entry
    }
}

#[repr(C)]
struct SpanSlot {
    state: SpanSlotState,
    record: MaybeUninit<SpanRecord>,
}

impl SpanSlot {
    fn is_empty(&self) -> bool {
        self.state == SpanSlotState::Empty
    }

    fn set(&mut self, record: SpanRecord) {
        self.record.write(record);
        self.state = SpanSlotState::Occupied;
    }

    fn clear(&mut self) {
        self.state = SpanSlotState::Empty;
    }

    fn record(&self) -> Option<SpanRecord> {
        if self.is_empty() {
            return None;
        }

        // SAFETY: Occupied state is set only after record.write initializes this slot.
        Some(unsafe { *self.record.assume_init_ref() })
    }

    fn record_containing(&self, index: L2Index) -> Option<SpanRecord> {
        self.record().filter(|record| record.contains(index))
    }

    fn overlaps(&self, segment: L2Segment) -> bool {
        self.record().is_some_and(|record| record.overlaps(segment))
    }

    fn covers(&self, segment: L2Segment) -> bool {
        self.record()
            .is_some_and(|record| record.segment().range() == segment.range())
    }

    fn matches(&self, segment: L2Segment, entry: MapEntry) -> bool {
        self.record()
            .is_some_and(|record| record.matches(segment, entry))
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum SpanSlotState {
    Empty = 0,
    Occupied = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

        self.l1()?.page_entry(l1_index, l2_index)?.page()
    }

    pub(crate) fn insert(
        &mut self,
        range: PageRange,
        entry: PageEntry,
    ) -> Result<(), PageMapError> {
        let occupied = MapEntry::occupied(entry).ok_or(PageMapError::InvalidRange)?;

        self.validate_insert(range)?;
        self.prepare_insert(range)?;

        let result = if let Some(l1) = self.l1_mut() {
            let mut result = Ok(());

            for segment in range.segments() {
                let published = match entry {
                    PageEntry::Run(_) => l1
                        .entry_mut(segment.l1)?
                        .assign_direct(segment.l2, occupied),
                    PageEntry::Extent(_) => {
                        l1.entry_mut(segment.l1)?.assign_span(segment.l2, occupied)
                    }
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
            self.release_empty_l2_tables(range);

            return Err(error);
        }

        Ok(())
    }

    pub(crate) fn remove(
        &mut self,
        range: PageRange,
        expected: PageEntry,
        l2_table_policy: L2TablePolicy,
    ) -> Result<(), PageMapError> {
        self.validate_remove(range, expected)?;

        let l1 = self.l1_mut().ok_or(PageMapError::UnexpectedEntry)?;
        for segment in range.segments() {
            l1.entry_mut(segment.l1)?.clear_segment(segment.l2)?;
        }

        if l2_table_policy == L2TablePolicy::ReleaseEmpty {
            self.release_empty_l2_tables(range);
        }

        Ok(())
    }

    fn rollback_insert(&mut self, range: PageRange, entry: MapEntry) {
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
                .entry_mut(segment.l1)
                .and_then(|entry_slot| entry_slot.clear_segment(segment.l2));
        }
    }

    fn release_empty_l2_tables(&mut self, range: PageRange) {
        for segment in range.segments() {
            if let Some(l1) = self.l1_mut() {
                let _ = l1.release_empty_l2(segment.l1);
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

    fn validate_remove(&self, range: PageRange, expected: PageEntry) -> Result<(), PageMapError> {
        let expected = MapEntry::occupied(expected).ok_or(PageMapError::InvalidRange)?;

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

    fn prepare_insert(&mut self, range: PageRange) -> Result<(), PageMapError> {
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

        if let Err(error) = result {
            self.release_empty_l2_tables(range);
            return Err(error);
        }

        Ok(())
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

        map.l1()
            .and_then(|l1| l1.entries.get(l1_index.get()))
            .is_some_and(L1Entry::has_l2_table)
    }

    fn l2_table_for(map: &PageMap, ptr: NonNull<u8>) -> Option<&L2Table> {
        let (l1_index, _) = Page::containing(ptr).indexes()?;

        map.l1()?.entries.get(l1_index.get())?.l2_table_ref()
    }

    fn span_count(map: &PageMap, ptr: NonNull<u8>) -> usize {
        l2_table_for(map, ptr).map_or(0, |table| {
            table.spans.iter().filter(|slot| !slot.is_empty()).count()
        })
    }

    fn direct_entry(map: &PageMap, ptr: NonNull<u8>) -> Option<MapEntry> {
        let (_, l2_index) = Page::containing(ptr).indexes()?;

        l2_table_for(map, ptr)?.pages.get(l2_index.get()).copied()
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

        fn first_l2_boundary_offset(&self) -> usize {
            let (_, base_l2) = Page::containing(self.base()).indexes().unwrap();

            (L2_ENTRIES - base_l2.get()) * PAGE_SIZE
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
    fn page_map_insert_extent_range_uses_span_record() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, extent(4)).is_ok());

        assert_eq!(span_count(&map, mapping.base()), 1);
        assert_eq!(direct_entry(&map, mapping.base()), Some(MapEntry::empty()));
        assert_eq!(
            direct_entry(&map, mapping.ptr_at(PAGE_SIZE)),
            Some(MapEntry::empty())
        );
    }

    #[test]
    fn page_map_insert_run_range_uses_direct_entries() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(4)).is_ok());

        assert_eq!(span_count(&map, mapping.base()), 0);
        assert_eq!(
            direct_entry(&map, mapping.base()),
            MapEntry::occupied(run(4))
        );
        assert_eq!(
            direct_entry(&map, mapping.ptr_at(PAGE_SIZE)),
            MapEntry::occupied(run(4))
        );
    }

    #[test]
    fn page_map_remove_range_clears_mapped_pages() {
        let mapping = TestMapping::new(PAGE_SIZE * 2);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(8)).is_ok());
        assert_eq!(
            map.remove(range, run(8), L2TablePolicy::ReleaseEmpty),
            Ok(())
        );

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

        assert_eq!(
            map.remove(range, run(1), L2TablePolicy::ReleaseEmpty),
            Ok(())
        );

        assert!(map.get(mapping.base()).is_none());
        assert!(!has_l2_table(&map, mapping.base()));
    }

    #[test]
    fn page_map_remove_range_can_retain_empty_l2_table() {
        let mapping = TestMapping::new(PAGE_SIZE);
        let mut map = PageMap::new();
        let range = mapping.page_range();

        assert!(map.insert(range, run(1)).is_ok());
        assert!(has_l2_table(&map, mapping.base()));

        assert_eq!(
            map.remove(range, run(1), L2TablePolicy::RetainEmpty),
            Ok(())
        );

        assert!(map.get(mapping.base()).is_none());
        assert!(has_l2_table(&map, mapping.base()));
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
                L2TablePolicy::ReleaseEmpty,
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
                L2TablePolicy::ReleaseEmpty,
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
            map.remove(range, run(2), L2TablePolicy::ReleaseEmpty),
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
            map.remove(mapping.page_range(), run(1), L2TablePolicy::ReleaseEmpty),
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
            map.remove(mapping.page_range(), run(1), L2TablePolicy::ReleaseEmpty),
            Err(PageMapError::UnexpectedEntry)
        );
        assert_eq!(map.get(first), Some(run(1)));
        assert_eq!(map.get(second), Some(run(2)));
    }

    #[test]
    fn page_map_remove_range_rejects_cross_l2_partial_mismatch_without_clearing() {
        let mapping = TestMapping::new((L2_ENTRIES + 2) * PAGE_SIZE);
        let mut map = PageMap::new();
        let boundary = mapping.first_l2_boundary_offset();
        let before_boundary = mapping.ptr_at(boundary - PAGE_SIZE);
        let after_boundary = mapping.ptr_at(boundary);

        assert!(
            map.insert(PageRange::new(before_boundary, PAGE_SIZE).unwrap(), run(1))
                .is_ok()
        );
        assert!(
            map.insert(PageRange::new(after_boundary, PAGE_SIZE).unwrap(), run(2))
                .is_ok()
        );

        assert_eq!(
            map.remove(
                PageRange::new(before_boundary, PAGE_SIZE * 2).unwrap(),
                run(1),
                L2TablePolicy::ReleaseEmpty,
            ),
            Err(PageMapError::UnexpectedEntry)
        );

        assert_eq!(map.get(before_boundary), Some(run(1)));
        assert_eq!(map.get(after_boundary), Some(run(2)));
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

    #[test]
    fn page_map_insert_extent_range_crosses_l2_boundary_with_spans() {
        let len = (L2_ENTRIES + 2) * PAGE_SIZE;
        let mapping = TestMapping::new(len);
        let mut map = PageMap::new();
        let range = mapping.page_range();
        let boundary = mapping.ptr_at(mapping.first_l2_boundary_offset());
        let last = mapping.ptr_at(mapping.len() - 1);

        assert!(map.insert(range, extent(10)).is_ok());

        assert_eq!(map.get(mapping.base()), Some(extent(10)));
        assert_eq!(map.get(boundary), Some(extent(10)));
        assert_eq!(map.get(last), Some(extent(10)));
        assert_eq!(span_count(&map, mapping.base()), 1);
        assert_eq!(span_count(&map, boundary), 1);
    }

    #[test]
    fn page_map_extent_span_exhaustion_falls_back_to_direct_entries() {
        let mapping = TestMapping::new((SPAN_SLOTS + 1) * PAGE_SIZE);
        let mut map = PageMap::new();

        for index in 0..SPAN_SLOTS {
            let ptr = mapping.ptr_at(index * PAGE_SIZE);
            assert!(
                map.insert(
                    PageRange::new(ptr, PAGE_SIZE).unwrap(),
                    extent(u32::try_from(index).unwrap()),
                )
                .is_ok()
            );
        }

        let fallback = mapping.ptr_at(SPAN_SLOTS * PAGE_SIZE);
        assert!(
            map.insert(PageRange::new(fallback, PAGE_SIZE).unwrap(), extent(10_000),)
                .is_ok()
        );

        assert_eq!(span_count(&map, mapping.base()), SPAN_SLOTS);
        assert_eq!(map.get(fallback), Some(extent(10_000)));
        assert_eq!(
            direct_entry(&map, fallback),
            MapEntry::occupied(extent(10_000))
        );
        assert_eq!(
            map.remove(
                PageRange::new(fallback, PAGE_SIZE).unwrap(),
                extent(10_000),
                L2TablePolicy::RetainEmpty,
            ),
            Ok(())
        );
        assert!(map.get(fallback).is_none());
    }

    #[test]
    fn page_map_remove_range_crosses_l2_boundary() {
        let len = (L2_ENTRIES + 2) * PAGE_SIZE;
        let mapping = TestMapping::new(len);
        let mut map = PageMap::new();
        let range = mapping.page_range();
        let boundary = mapping.ptr_at(mapping.first_l2_boundary_offset());
        let last = mapping.ptr_at(mapping.len() - 1);

        assert!(map.insert(range, run(10)).is_ok());
        assert_eq!(
            map.remove(range, run(10), L2TablePolicy::ReleaseEmpty),
            Ok(())
        );

        assert!(map.get(mapping.base()).is_none());
        assert!(map.get(boundary).is_none());
        assert!(map.get(last).is_none());
    }
}
