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

fn direct_entry(map: &PageMap, ptr: NonNull<u8>) -> Option<MapEntry> {
    let (_, l2_index) = Page::containing(ptr).indexes()?;

    l2_table_for(map, ptr)?
        .pages
        .get(l2_index.get())
        .map(AtomicMapEntry::load)
}

fn insert(map: &PageMap, range: PageRange, owner: PageOwner) -> Result<(), PageMapError> {
    let mut l1_mapping = map.l1_mapping.lock();
    map.insert(&mut l1_mapping, range, owner)
}

fn remove(map: &PageMap, range: PageRange, owner: PageOwner) -> Result<(), PageMapError> {
    let mut l1_mapping = map.l1_mapping.lock();
    map.remove(&mut l1_mapping, range, owner)
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
        PageRange::from_mapping(&self.mapping).unwrap()
    }

    fn mapping(&self) -> &crate::memory::Mapping {
        &self.mapping
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

    assert!(insert(&map, range, run(7)).is_ok());

    let interior = mapping.ptr_at(PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(run(7)));
}

#[test]
fn page_map_insert_range_maps_extent_entry() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, extent(4)).is_ok());

    let interior = mapping.ptr_at(PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(extent(4)));
}

#[test]
fn page_map_insert_extent_range_uses_direct_entries() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, extent(4)).is_ok());

    assert_eq!(
        direct_entry(&map, mapping.base()),
        MapEntry::from_owner(extent(4))
    );
    assert_eq!(
        direct_entry(&map, mapping.ptr_at(PAGE_SIZE)),
        MapEntry::from_owner(extent(4))
    );
}

#[test]
fn page_map_insert_run_range_uses_direct_entries() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(4)).is_ok());

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

    assert!(insert(&map, range, run(8)).is_ok());
    assert_eq!(remove(&map, range, run(8)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    let second = mapping.ptr_at(PAGE_SIZE);
    assert!(map.get(second).is_none());
}

#[test]
fn page_map_remove_range_retains_empty_l2_table_for_stable_reads() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(1)).is_ok());
    assert!(has_l2_table(&map, mapping.base()));

    assert_eq!(remove(&map, range, run(1)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(has_l2_table(&map, mapping.base()));
}

#[test]
fn page_map_remove_range_can_retain_empty_l2_table() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(1)).is_ok());
    assert!(has_l2_table(&map, mapping.base()));

    assert_eq!(remove(&map, range, run(1)), Ok(()));

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
        insert(
            &map,
            PageRange::from_aligned(first, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );
    assert!(
        insert(
            &map,
            PageRange::from_aligned(second, PAGE_SIZE).unwrap(),
            run(2)
        )
        .is_ok()
    );

    assert_eq!(
        remove(
            &map,
            PageRange::from_aligned(first, PAGE_SIZE).unwrap(),
            run(1),
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
    let map = PageMap::new();
    let first = mapping.base();
    let second = mapping.ptr_at(PAGE_SIZE);
    let third = mapping.ptr_at(PAGE_SIZE * 2);

    assert!(
        insert(
            &map,
            PageRange::from_aligned(first, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );
    assert!(
        insert(
            &map,
            PageRange::from_aligned(second, PAGE_SIZE).unwrap(),
            run(2)
        )
        .is_ok()
    );
    assert!(
        insert(
            &map,
            PageRange::from_aligned(third, PAGE_SIZE).unwrap(),
            run(3)
        )
        .is_ok()
    );

    assert_eq!(
        remove(
            &map,
            PageRange::from_aligned(second, PAGE_SIZE).unwrap(),
            run(2),
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
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(1)).is_ok());

    assert_eq!(
        remove(&map, range, run(2)),
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
        insert(
            &map,
            PageRange::from_aligned(first, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );

    assert_eq!(
        remove(&map, mapping.page_range(), run(1)),
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
        insert(
            &map,
            PageRange::from_aligned(first, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );
    assert!(
        insert(
            &map,
            PageRange::from_aligned(second, PAGE_SIZE).unwrap(),
            run(2)
        )
        .is_ok()
    );

    assert_eq!(
        remove(&map, mapping.page_range(), run(1)),
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
        insert(
            &map,
            PageRange::from_aligned(before_boundary, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );
    assert!(
        insert(
            &map,
            PageRange::from_aligned(after_boundary, PAGE_SIZE).unwrap(),
            run(2)
        )
        .is_ok()
    );

    assert_eq!(
        remove(
            &map,
            PageRange::from_aligned(before_boundary, PAGE_SIZE * 2).unwrap(),
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
        insert(
            &map,
            PageRange::from_aligned(mapping.base(), PAGE_SIZE * 2).unwrap(),
            run(11),
        )
        .is_ok()
    );
    assert_eq!(
        insert(
            &map,
            PageRange::from_aligned(second, PAGE_SIZE).unwrap(),
            run(12)
        ),
        Err(PageMapError::Overlap)
    );
    assert_eq!(map.get(second), Some(run(11)));
}

#[test]
fn page_map_insert_range_rejects_existing_same_entry() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(13)).is_ok());
    assert_eq!(insert(&map, range, run(13)), Err(PageMapError::Overlap));
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
        insert(
            &map,
            PageRange::from_aligned(overlap, PAGE_SIZE).unwrap(),
            run(21)
        )
        .is_ok()
    );
    assert!(!has_l2_table(&map, mapping.base()));

    assert_eq!(
        insert(
            &map,
            PageRange::from_aligned(mapping.base(), (pages_to_next_l2 + 1) * PAGE_SIZE).unwrap(),
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

    assert!(PageRange::from_aligned(mapping.base(), 0).is_none());
    assert!(PageRange::from_aligned(mapping.base(), PAGE_SIZE / 2).is_none());
    let unaligned = NonNull::new(mapping.base().as_ptr().wrapping_add(1)).unwrap();
    assert!(PageRange::from_aligned(unaligned, PAGE_SIZE).is_none());
}

#[test]
fn page_map_insert_range_crosses_l2_boundary() {
    let len = (L2_ENTRIES + 2) * PAGE_SIZE;
    let mapping = TestMapping::new(len);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(insert(&map, range, run(10)).is_ok());

    let last = mapping.ptr_at(mapping.len() - 1);
    assert_eq!(map.get(mapping.base()), Some(run(10)));
    assert_eq!(map.get(last), Some(run(10)));
}

#[test]
fn page_map_insert_extent_range_crosses_l2_boundary() {
    let len = (L2_ENTRIES + 2) * PAGE_SIZE;
    let mapping = TestMapping::new(len);
    let map = PageMap::new();
    let range = mapping.page_range();
    let boundary = mapping.ptr_at(mapping.first_l2_boundary_offset());
    let last = mapping.ptr_at(mapping.len() - 1);

    assert!(insert(&map, range, extent(10)).is_ok());

    assert_eq!(map.get(mapping.base()), Some(extent(10)));
    assert_eq!(map.get(boundary), Some(extent(10)));
    assert_eq!(map.get(last), Some(extent(10)));
}

/// A single L2 table has 4096 page slots; many more than 64 (the old span-slot
/// bound) single-page extents must coexist in one table without failing, since
/// extents now use the same unbounded direct-entry representation as runs.
#[test]
fn page_map_many_single_page_extents_share_one_l2_table_without_exhaustion() {
    const EXTENT_COUNT: usize = 200;
    let mapping = TestMapping::new(EXTENT_COUNT * PAGE_SIZE);
    let map = PageMap::new();

    for index in 0..EXTENT_COUNT {
        let ptr = mapping.ptr_at(index * PAGE_SIZE);
        assert!(
            insert(
                &map,
                PageRange::from_aligned(ptr, PAGE_SIZE).unwrap(),
                extent(u32::try_from(index).unwrap()),
            )
            .is_ok()
        );
    }

    for index in 0..EXTENT_COUNT {
        let ptr = mapping.ptr_at(index * PAGE_SIZE);
        assert_eq!(map.get(ptr), Some(extent(u32::try_from(index).unwrap())));
    }
}

#[test]
fn page_map_publish_extent_unpublish_extent_round_trip() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let owner = owner_ptr(2);

    map.publish_extent(mapping.mapping(), owner).unwrap();
    assert_eq!(map.get(mapping.base()), Some(PageOwner::Extent(owner)));

    map.unpublish_extent(mapping.mapping(), owner).unwrap();
    assert!(map.get(mapping.base()).is_none());
}

#[test]
fn page_map_remove_range_crosses_l2_boundary() {
    let len = (L2_ENTRIES + 2) * PAGE_SIZE;
    let mapping = TestMapping::new(len);
    let map = PageMap::new();
    let range = mapping.page_range();
    let boundary = mapping.ptr_at(mapping.first_l2_boundary_offset());
    let last = mapping.ptr_at(mapping.len() - 1);

    assert!(insert(&map, range, run(10)).is_ok());
    assert_eq!(remove(&map, range, run(10)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(map.get(boundary).is_none());
    assert!(map.get(last).is_none());
}
