use super::table::L2Table;
use super::*;
use crate::{
    config::AllocatorConfig,
    heap::{
        Extent, Heap, HeapId, Run, RunId,
        extent::ExtentId,
        run::{RUN_SIZE, RUN_SPACE, config::RunPolicy},
    },
    layout::LayoutSpec,
    size_class::SizeClasses,
};
use core::{alloc::Layout, num::NonZeroU32, ptr::NonNull};
use std::sync::{Barrier, OnceLock};

const OWNER_COUNT: usize = 256;
static HEAPS: [OnceLock<Heap>; OWNER_COUNT] = [const { OnceLock::new() }; OWNER_COUNT];
static RUN_MAPS: [OnceLock<Mapping>; OWNER_COUNT] = [const { OnceLock::new() }; OWNER_COUNT];
static RUNS: [OnceLock<&'static Run>; OWNER_COUNT] = [const { OnceLock::new() }; OWNER_COUNT];
static EXTENTS: [OnceLock<Extent>; OWNER_COUNT] = [const { OnceLock::new() }; OWNER_COUNT];

fn heap(raw: u32) -> &'static Heap {
    HEAPS[usize::try_from(raw).unwrap()].get_or_init(|| {
        Heap::new(
            HeapId::new(raw, NonZeroU32::MIN).unwrap(),
            AllocatorConfig::new(),
        )
    })
}

fn run(raw: u32) -> PageOwner {
    let i = usize::try_from(raw).unwrap();
    PageOwner::Run(RUNS[i].get_or_init(|| {
        let mapping = Os::map_aligned(RUN_SPACE, RUN_SIZE).unwrap();
        let base = mapping.base();
        let _ = RUN_MAPS[i].set(mapping);
        let spec = LayoutSpec::from_layout(Layout::from_size_align(64, 8).unwrap());
        let class = SizeClasses::class_for(spec).unwrap();
        let run = Run::new(
            RunId::from_index(raw).unwrap(),
            heap(raw),
            base,
            class,
            RunPolicy::Keep,
        )
        .unwrap();
        let header = base.cast::<Run>().as_ptr().wrapping_byte_add(RUN_SIZE);
        // SAFETY: mapped `RUN_SPACE` tail; first write of this in-space header.
        unsafe { header.write(run) };
        // SAFETY: just written; mapping retained in `RUN_MAPS`.
        unsafe { &*header }
    }))
}

fn extent(raw: u32) -> PageOwner {
    PageOwner::Extent(EXTENTS[usize::try_from(raw).unwrap()].get_or_init(|| {
        let spec = LayoutSpec::from_layout(Layout::from_size_align(PAGE_SIZE, 8).unwrap());
        let mapping = Os::map(PAGE_SIZE).unwrap();
        Extent::new(ExtentId::from_index(raw).unwrap(), heap(raw), mapping, spec).unwrap()
    }))
}

fn has_l2_table(map: &PageMap, ptr: NonNull<u8>) -> bool {
    let Some((l1_index, _)) = Page::split(ptr) else {
        return false;
    };
    map.l1()
        .is_some_and(|l1| l1.l2_table_ref(l1_index).is_some())
}

fn l2_table_for(map: &PageMap, ptr: NonNull<u8>) -> Option<&L2Table> {
    let (l1_index, _) = Page::split(ptr)?;
    map.l1()?.l2_table_ref(l1_index)
}

fn direct_entry(map: &PageMap, ptr: NonNull<u8>) -> Option<MapEntry> {
    let (_, l2_index) = Page::split(ptr)?;
    Some(l2_table_for(map, ptr)?.entry(l2_index).load())
}

/// Page inside `mapping` at `offset` bytes from its base.
fn page_at(mapping: &Mapping, offset: usize) -> NonNull<u8> {
    assert!(offset < mapping.len().get());

    NonNull::new(mapping.base().as_ptr().wrapping_add(offset)).unwrap()
}

/// Offset from `mapping`'s base to the first page of the next L2 table.
fn l2_boundary_offset(mapping: &Mapping) -> usize {
    let (_, base_l2) = Page::split(mapping.base()).unwrap();

    (L2_ENTRIES - base_l2.get()) * PAGE_SIZE
}

#[test]
fn page_map_new_lookup_returns_none() {
    let map = PageMap::new();
    let ptr = NonNull::dangling();

    assert!(map.get(ptr).is_none());
}

#[test]
fn page_map_get_rejects_out_of_addressable_page() {
    let map = PageMap::new();
    let addr = ADDRESSABLE_PAGES << PAGE_SHIFT;
    let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(addr)).unwrap();

    assert!(Page::split(ptr).is_none());
    assert!(map.get(ptr).is_none());
}

#[test]
fn page_map_insert_range_maps_interior_pointer() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(7)).is_ok());

    let interior = page_at(&mapping, PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(run(7)));
}

#[test]
fn page_map_insert_range_maps_extent_entry() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, extent(4)).is_ok());

    let interior = page_at(&mapping, PAGE_SIZE + 17);
    assert_eq!(map.get(interior), Some(extent(4)));
}

#[test]
fn page_map_insert_extent_range_uses_direct_entries() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, extent(4)).is_ok());

    assert_eq!(
        direct_entry(&map, mapping.base()),
        MapEntry::from_owner(extent(4))
    );
    assert_eq!(
        direct_entry(&map, page_at(&mapping, PAGE_SIZE)),
        MapEntry::from_owner(extent(4))
    );
}

#[test]
fn page_map_insert_run_range_uses_direct_entries() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(4)).is_ok());

    assert_eq!(
        direct_entry(&map, mapping.base()),
        MapEntry::from_owner(run(4))
    );
    assert_eq!(
        direct_entry(&map, page_at(&mapping, PAGE_SIZE)),
        MapEntry::from_owner(run(4))
    );
}

#[test]
fn page_map_remove_range_clears_mapped_pages() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(8)).is_ok());
    assert_eq!(map.remove(range, run(8)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    let second = page_at(&mapping, PAGE_SIZE);
    assert!(map.get(second).is_none());
}

#[test]
fn page_map_remove_range_retains_empty_l2_table_for_stable_reads() {
    let mapping = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(1)).is_ok());
    assert!(has_l2_table(&map, mapping.base()));

    assert_eq!(map.remove(range, run(1)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(has_l2_table(&map, mapping.base()));
}

#[test]
fn page_map_remove_rejects_never_published_range_and_keeps_existing() {
    let published = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    assert!(
        map.insert(PageRange::from_mapping(&published).unwrap(), extent(1))
            .is_ok()
    );

    let stranger = Os::map(PAGE_SIZE).unwrap();
    assert_eq!(
        map.remove(PageRange::from_mapping(&stranger).unwrap(), extent(2)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(published.base()), Some(extent(1)));
    assert!(map.get(stranger.base()).is_none());
}

#[test]
fn page_map_remove_range_keeps_non_empty_l2_table() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let first = mapping.base();
    let second = page_at(&mapping, PAGE_SIZE);

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
    let mapping = Os::map(PAGE_SIZE * 3).unwrap();
    let map = PageMap::new();
    let first = mapping.base();
    let second = page_at(&mapping, PAGE_SIZE);
    let third = page_at(&mapping, PAGE_SIZE * 2);

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
    let mapping = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(1)).is_ok());

    assert_eq!(
        map.remove(range, run(2)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(mapping.base()), Some(run(1)));
}

#[test]
fn page_map_remove_range_rejects_missing_entry_without_clearing() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let first = mapping.base();
    let second = page_at(&mapping, PAGE_SIZE);

    assert!(
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );

    assert_eq!(
        map.remove(PageRange::from_mapping(&mapping).unwrap(), run(1)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(first), Some(run(1)));
    assert!(map.get(second).is_none());
}

#[test]
fn page_map_remove_range_rejects_partial_mismatch_without_clearing() {
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let first = mapping.base();
    let second = page_at(&mapping, PAGE_SIZE);

    assert!(
        map.insert(PageRange::from_aligned(first, PAGE_SIZE).unwrap(), run(1))
            .is_ok()
    );
    assert!(
        map.insert(PageRange::from_aligned(second, PAGE_SIZE).unwrap(), run(2))
            .is_ok()
    );

    assert_eq!(
        map.remove(PageRange::from_mapping(&mapping).unwrap(), run(1)),
        Err(PageMapError::UnexpectedEntry)
    );
    assert_eq!(map.get(first), Some(run(1)));
    assert_eq!(map.get(second), Some(run(2)));
}

#[test]
fn page_map_remove_range_rejects_cross_l2_partial_mismatch_without_clearing() {
    let mapping = Os::map((L2_ENTRIES + 2) * PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let boundary = l2_boundary_offset(&mapping);
    let before_boundary = page_at(&mapping, boundary - PAGE_SIZE);
    let after_boundary = page_at(&mapping, boundary);

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
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let second = page_at(&mapping, PAGE_SIZE);

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
    let mapping = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(13)).is_ok());
    assert_eq!(map.insert(range, run(13)), Err(PageMapError::Overlap));
    assert_eq!(map.get(mapping.base()), Some(run(13)));
}

#[test]
fn page_map_overlap_rejects_under_write_exclusion_and_retains_l2() {
    let mapping = Os::map((L2_ENTRIES * 2 + 2) * PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let pages_to_next_l2 = l2_boundary_offset(&mapping) / PAGE_SIZE;
    let overlap = page_at(&mapping, pages_to_next_l2 * PAGE_SIZE);

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

    // Failed insert may publish the base L2 while ensuring metadata; validate-then-store under
    // write exclusion writes nothing on overlap. Installed L2 is retained for the PageMap
    // lifetime; rejected pages must read as empty.
    assert_eq!(map.get(mapping.base()), None);
    assert_eq!(map.get(overlap), Some(run(21)));
}

#[test]
fn page_map_insert_range_rejects_zero_len() {
    let mapping = Os::map(PAGE_SIZE).unwrap();

    assert!(PageRange::from_aligned(mapping.base(), 0).is_none());
    assert!(PageRange::from_aligned(mapping.base(), PAGE_SIZE / 2).is_none());
    let unaligned = NonNull::new(mapping.base().as_ptr().wrapping_add(1)).unwrap();
    assert!(PageRange::from_aligned(unaligned, PAGE_SIZE).is_none());
}

#[test]
fn page_map_insert_range_crosses_l2_boundary() {
    let mapping = Os::map((L2_ENTRIES + 2) * PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();

    assert!(map.insert(range, run(10)).is_ok());

    let last = page_at(&mapping, mapping.len().get() - 1);
    assert_eq!(map.get(mapping.base()), Some(run(10)));
    assert_eq!(map.get(last), Some(run(10)));
}

#[test]
fn page_map_insert_extent_range_crosses_l2_boundary() {
    let mapping = Os::map((L2_ENTRIES + 2) * PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();
    let boundary = page_at(&mapping, l2_boundary_offset(&mapping));
    let last = page_at(&mapping, mapping.len().get() - 1);

    assert!(map.insert(range, extent(10)).is_ok());

    assert_eq!(map.get(mapping.base()), Some(extent(10)));
    assert_eq!(map.get(boundary), Some(extent(10)));
    assert_eq!(map.get(last), Some(extent(10)));
}

/// Many single-page extents share one L2 via direct per-page entries.
#[test]
fn page_map_many_single_page_extents_share_one_l2_table_without_exhaustion() {
    const EXTENT_COUNT: usize = 200;
    let mapping = Os::map(EXTENT_COUNT * PAGE_SIZE).unwrap();
    let map = PageMap::new();

    for index in 0..EXTENT_COUNT {
        let ptr = page_at(&mapping, index * PAGE_SIZE);
        assert!(
            map.insert(
                PageRange::from_aligned(ptr, PAGE_SIZE).unwrap(),
                extent(u32::try_from(index).unwrap()),
            )
            .is_ok()
        );
    }

    for index in 0..EXTENT_COUNT {
        let ptr = page_at(&mapping, index * PAGE_SIZE);
        assert_eq!(map.get(ptr), Some(extent(u32::try_from(index).unwrap())));
    }
}

#[test]
fn page_map_publish_run_stamps_payload_not_claim_tail() {
    let map = PageMap::new();
    let owner = run(7);
    let PageOwner::Run(header) = owner else {
        unreachable!()
    };
    let base = header.range().base();

    map.publish(owner).unwrap();

    assert_eq!(map.get(base), Some(owner));
    let tail = NonNull::new(base.as_ptr().wrapping_add(RUN_SIZE)).unwrap();
    assert!(map.get(tail).is_none());
}

#[test]
fn page_map_publish_unpublish_round_trip() {
    let map = PageMap::new();
    let owner = extent(2);
    let PageOwner::Extent(slot) = owner else {
        unreachable!()
    };
    let base = slot.mapping().base();

    map.publish(owner).unwrap();
    assert_eq!(map.get(base), Some(owner));

    map.unpublish(owner).unwrap();
    assert!(map.get(base).is_none());
}

#[test]
fn page_map_remove_range_crosses_l2_boundary() {
    let mapping = Os::map((L2_ENTRIES + 2) * PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();
    let boundary = page_at(&mapping, l2_boundary_offset(&mapping));
    let last = page_at(&mapping, mapping.len().get() - 1);

    assert!(map.insert(range, run(10)).is_ok());
    assert_eq!(map.remove(range, run(10)), Ok(()));

    assert!(map.get(mapping.base()).is_none());
    assert!(map.get(boundary).is_none());
    assert!(map.get(last).is_none());
}

#[test]
fn page_map_concurrent_disjoint_publish() {
    let left = Os::map(PAGE_SIZE).unwrap();
    let right = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    // Copy ranges/bases: `Mapping` is `Send` but not `Sync`, so threads must not borrow it.
    let left_range = PageRange::from_mapping(&left).unwrap();
    let right_range = PageRange::from_mapping(&right).unwrap();
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
    let mapping = Os::map(PAGE_SIZE * 2).unwrap();
    let map = PageMap::new();
    let start = Barrier::new(2);
    let first_base = mapping.base();
    let second_base = page_at(&mapping, PAGE_SIZE);
    let first = PageRange::from_aligned(first_base, PAGE_SIZE).unwrap();
    let second = PageRange::from_aligned(second_base, PAGE_SIZE).unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            assert_eq!(map.insert(first, run(1)), Ok(()));
        });
        scope.spawn(|| {
            start.wait();
            assert_eq!(map.insert(second, run(2)), Ok(()));
        });
    });

    assert_eq!(map.get(first_base), Some(run(1)));
    assert_eq!(map.get(second_base), Some(run(2)));
}

#[test]
fn page_map_concurrent_overlap_exactly_one_wins() {
    let mapping = Os::map(PAGE_SIZE).unwrap();
    let map = PageMap::new();
    let range = PageRange::from_mapping(&mapping).unwrap();
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
