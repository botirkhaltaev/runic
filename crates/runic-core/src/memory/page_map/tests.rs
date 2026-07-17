use super::*;
use super::{
    entry::AtomicMapEntry,
    table::{L1Entry, L2Table},
};

fn owner_ptr<T>(raw: u32) -> NonNull<T> {
    let addr = (usize::try_from(raw).unwrap() + 1) << 4;
    NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap()
}

fn run(raw: u32) -> PageOwner {
    PageOwner::Run(owner_ptr(raw))
}

fn extent(raw: u32) -> PageOwner {
    PageOwner::Extent(owner_ptr(raw))
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

    l2_table_for(map, ptr)?
        .pages
        .get(l2_index.get())
        .map(AtomicMapEntry::load)
}

struct TestMapping {
    mapping: crate::memory::Mapping,
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
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(7)).is_ok());

    let interior = mapping.ptr_at(PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(run(7)));
}

#[test]
fn page_map_insert_range_maps_extent_entry() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, extent(4)).is_ok());

    let interior = mapping.ptr_at(PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(extent(4)));
}

#[test]
fn page_map_insert_extent_range_uses_span_record() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
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
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(4)).is_ok());

    assert_eq!(span_count(&map, mapping.base()), 0);
    assert_eq!(
        direct_entry(&map, mapping.base()),
        MapEntry::from_owner(run(4))
    );
    assert_eq!(
        direct_entry(&map, mapping.ptr_at(PAGE_SIZE)),
        MapEntry::from_owner(run(4))
    );
}

#[test]
fn page_map_remove_range_clears_mapped_pages() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(8)).is_ok());
    assert_eq!(map.remove(range, run(8)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    let second = mapping.ptr_at(PAGE_SIZE);
    assert!(map.get(second).is_none());
}

#[test]
fn page_map_remove_range_retains_empty_l2_table_for_stable_reads() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(1)).is_ok());
    assert!(has_l2_table(&map, mapping.base()));

    assert_eq!(map.remove(range, run(1)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(has_l2_table(&map, mapping.base()));
}

#[test]
fn page_map_remove_range_can_retain_empty_l2_table() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(1)).is_ok());
    assert!(has_l2_table(&map, mapping.base()));

    assert_eq!(map.remove(range, run(1)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(has_l2_table(&map, mapping.base()));
}

#[test]
fn page_map_remove_range_keeps_non_empty_l2_table() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
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
        map.remove(PageRange::new(first, PAGE_SIZE).unwrap(), run(1),),
        Ok(())
    );

    assert!(map.get(first).is_none());
    assert_eq!(map.get(second), Some(run(2)));
    assert!(has_l2_table(&map, second));
}

#[test]
fn page_map_remove_range_preserves_neighboring_page() {
    let mapping = TestMapping::new(PAGE_SIZE * 3);
    let map = PageMap::new();
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
        map.remove(PageRange::new(second, PAGE_SIZE).unwrap(), run(2),),
        Ok(())
    );

    assert_eq!(map.get(first), Some(run(1)));
    assert!(map.get(second).is_none());
    assert_eq!(map.get(third), Some(run(3)));
}

#[test]
fn page_map_remove_range_rejects_wrong_owner_without_clearing() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(1)).is_ok());

    assert_eq!(
        map.remove(range, run(2)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(mapping.base()), Some(run(1)));
}

#[test]
fn page_map_remove_range_rejects_missing_entry_without_clearing() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let first = mapping.base();
    let second = mapping.ptr_at(PAGE_SIZE);

    assert!(
        map.insert(PageRange::new(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );

    assert_eq!(
        map.remove(mapping.page_range(), run(1)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(first), Some(run(1)));
    assert!(map.get(second).is_none());
}

#[test]
fn page_map_remove_range_rejects_partial_mismatch_without_clearing() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
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
        map.remove(mapping.page_range(), run(1)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(first), Some(run(1)));
    assert_eq!(map.get(second), Some(run(2)));
}

#[test]
fn page_map_remove_range_rejects_cross_l2_partial_mismatch_without_clearing() {
    let mapping = TestMapping::new((L2_ENTRIES + 2) * PAGE_SIZE);
    let map = PageMap::new();
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
        ),
        Err(PageMapError::UnexpectedEntry)
    );

    assert_eq!(map.get(before_boundary), Some(run(1)));
    assert_eq!(map.get(after_boundary), Some(run(2)));
}

#[test]
fn page_map_insert_range_rejects_overlapping_different_run() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
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
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, run(13)).is_ok());
    assert_eq!(map.insert(range, run(13)), Err(PageMapError::Overlap));
    assert_eq!(map.get(mapping.base()), Some(run(13)));
}

#[test]
fn page_map_overlap_validation_does_not_allocate_empty_l2_tables() {
    let mapping = TestMapping::new((L2_ENTRIES * 2 + 2) * PAGE_SIZE);
    let map = PageMap::new();
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
    let map = PageMap::new();
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
    let map = PageMap::new();
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
    let map = PageMap::new();

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
        MapEntry::from_owner(extent(10_000))
    );
    assert_eq!(
        map.remove(PageRange::new(fallback, PAGE_SIZE).unwrap(), extent(10_000),),
        Ok(())
    );
    assert!(map.get(fallback).is_none());
}

#[test]
fn page_map_remove_range_crosses_l2_boundary() {
    let len = (L2_ENTRIES + 2) * PAGE_SIZE;
    let mapping = TestMapping::new(len);
    let map = PageMap::new();
    let range = mapping.page_range();
    let boundary = mapping.ptr_at(mapping.first_l2_boundary_offset());
    let last = mapping.ptr_at(mapping.len() - 1);

    assert!(map.insert(range, run(10)).is_ok());
    assert_eq!(map.remove(range, run(10)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(map.get(boundary).is_none());
    assert!(map.get(last).is_none());
}
