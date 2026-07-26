use super::table::L2Table;
use super::*;

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
    let (l1_index, _) = Page::split(ptr);
    map.l1()
        .is_some_and(|l1| l1.l2_table_ref(l1_index).is_some())
}

fn l2_table_for(map: &PageMap, ptr: NonNull<u8>) -> Option<&L2Table> {
    let (l1_index, _) = Page::split(ptr);
    map.l1()?.l2_table_ref(l1_index)
}

fn direct_entry(map: &PageMap, ptr: NonNull<u8>) -> Option<MapEntry> {
    let (_, l2_index) = Page::split(ptr);
    Some(l2_table_for(map, ptr)?.entry(l2_index))
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
        let (_, base_l2) = Page::split(self.base());

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
fn page_map_insert_extent_range_uses_direct_entries() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let range = mapping.page_range();

    assert!(map.insert(range, extent(4)).is_ok());

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

    assert!(map.insert(range, run(4)).is_ok());

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
fn page_map_remove_rejects_never_published_range_and_keeps_existing() {
    let published = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    assert!(map.insert(published.page_range(), extent(1)).is_ok());

    let stranger = TestMapping::new(PAGE_SIZE);
    assert_eq!(
        map.remove(stranger.page_range(), extent(2)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(published.base()), Some(extent(1)));
    assert!(map.get(stranger.base()).is_none());
}

#[test]
fn page_map_remove_range_keeps_non_empty_l2_table() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let first = mapping.base();
    let second = mapping.ptr_at(PAGE_SIZE);

    assert!(
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );
    assert!(
        map.insert(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(2))
            .is_ok()
    );

    assert_eq!(
        map.remove(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1),),
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
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );
    assert!(
        map.insert(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(2))
            .is_ok()
    );
    assert!(
        map.insert(PageRange::from_aligned(third, PAGE_SIZE).unwrap(), run(3))
            .is_ok()
    );

    assert_eq!(
        map.remove(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(2),),
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
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
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
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );
    assert!(
        map.insert(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(2))
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
        map.insert(
            PageRange::from_aligned(before_boundary, PAGE_SIZE).unwrap(),
            run(1)
        )
        .is_ok()
    );
    assert!(
        map.insert(
            PageRange::from_aligned(after_boundary, PAGE_SIZE).unwrap(),
            run(2)
        )
        .is_ok()
    );

    assert_eq!(
        map.remove(
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
        map.insert(
            PageRange::from_aligned(mapping.base(), PAGE_SIZE * 2).unwrap(),
            run(11),
        )
        .is_ok()
    );
    assert_eq!(
        map.insert(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(12)),
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
fn page_map_overlap_rejects_under_write_exclusion_and_retains_l2() {
    let mapping = TestMapping::new((L2_ENTRIES * 2 + 2) * PAGE_SIZE);
    let map = PageMap::new();
    let (_, base_l2) = Page::split(mapping.base());
    let pages_to_next_l2 = L2_ENTRIES - base_l2.get();
    let overlap = mapping.ptr_at(pages_to_next_l2 * PAGE_SIZE);

    assert!(
        map.insert(
            PageRange::from_aligned(overlap, PAGE_SIZE).unwrap(),
            run(21)
        )
        .is_ok()
    );
    assert!(!has_l2_table(&map, mapping.base()));

    assert_eq!(
        map.insert(
            PageRange::from_aligned(mapping.base(), (pages_to_next_l2 + 1) * PAGE_SIZE).unwrap(),
            run(22),
        ),
        Err(PageMapError::Overlap)
    );

    // Failed insert may install the base L2 during install_l2s; validate-then-store under
    // write exclusion writes nothing on overlap. Installed L2 is retained for the PageMap
    // lifetime; rejected pages must read as empty.
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

    assert!(map.insert(range, run(10)).is_ok());

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

    assert!(map.insert(range, extent(10)).is_ok());

    assert_eq!(map.get(mapping.base()), Some(extent(10)));
    assert_eq!(map.get(boundary), Some(extent(10)));
    assert_eq!(map.get(last), Some(extent(10)));
}

/// Many single-page extents share one L2 via direct per-page entries.
#[test]
fn page_map_many_single_page_extents_share_one_l2_table_without_exhaustion() {
    const EXTENT_COUNT: usize = 200;
    let mapping = TestMapping::new(EXTENT_COUNT * PAGE_SIZE);
    let map = PageMap::new();

    for index in 0..EXTENT_COUNT {
        let ptr = mapping.ptr_at(index * PAGE_SIZE);
        assert!(
            map.insert(
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

    assert!(map.insert(range, run(10)).is_ok());
    assert_eq!(map.remove(range, run(10)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(map.get(boundary).is_none());
    assert!(map.get(last).is_none());
}

#[test]
fn page_map_concurrent_disjoint_publish() {
    let left = TestMapping::new(PAGE_SIZE);
    let right = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    // Copy ranges/bases: `Mapping` is `Send` but not `Sync`, so threads must not borrow TestMapping.
    let left_range = left.page_range();
    let right_range = right.page_range();
    let left_base = left.base();
    let right_base = right.base();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert_eq!(map.insert(left_range, run(1)), Ok(()));
        });
        scope.spawn(|| {
            assert_eq!(map.insert(right_range, run(2)), Ok(()));
        });
    });

    assert_eq!(map.get(left_base), Some(run(1)));
    assert_eq!(map.get(right_base), Some(run(2)));
}

#[test]
fn page_map_concurrent_same_l2_disjoint_pages() {
    let mapping = TestMapping::new(PAGE_SIZE * 2);
    let map = PageMap::new();
    let first_base = mapping.base();
    let second_base = mapping.ptr_at(PAGE_SIZE);
    let first = PageRange::from_aligned(first_base, PAGE_SIZE).unwrap();
    let second = PageRange::from_aligned(second_base, PAGE_SIZE).unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert_eq!(map.insert(first, run(1)), Ok(()));
        });
        scope.spawn(|| {
            assert_eq!(map.insert(second, run(2)), Ok(()));
        });
    });

    assert_eq!(map.get(first_base), Some(run(1)));
    assert_eq!(map.get(second_base), Some(run(2)));
}

#[test]
fn page_map_concurrent_overlap_exactly_one_wins() {
    let mapping = TestMapping::new(PAGE_SIZE);
    let map = PageMap::new();
    let range = mapping.page_range();
    let base = mapping.base();

    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| map.insert(range, run(1)));
        let second = scope.spawn(|| map.insert(range, run(2)));
        (first.join().unwrap(), second.join().unwrap())
    });

    match (first, second) {
        (Ok(()), Err(PageMapError::Overlap)) => {
            assert_eq!(map.get(base), Some(run(1)));
        }
        (Err(PageMapError::Overlap), Ok(())) => {
            assert_eq!(map.get(base), Some(run(2)));
        }
        other => panic!("expected exactly one winner, got {other:?}"),
    }
}
