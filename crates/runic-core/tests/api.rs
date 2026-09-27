//! Single-thread contract of the public `Allocator` API, in the spirit of
//! mimalloc `test-api.c` and jemalloc `test/integration`.

mod common;

use core::alloc::Layout;
use std::collections::HashSet;

use common::{Block, CLASS_SIZES, EXTENT_SIZES, Rng, layout};
use runic_core::Allocator;

const SMALL_MAX: usize = CLASS_SIZES[CLASS_SIZES.len() - 1];

/// The class size a request lands in: the smallest class at or above
/// `max(size, align)` that is a multiple of `align`. `None` is an extent.
fn class_of(size: usize, align: usize) -> Option<usize> {
    if align > 4096 {
        return None;
    }
    CLASS_SIZES
        .into_iter()
        .find(|&class| class >= size.max(align) && class.is_multiple_of(align))
}

fn is_aligned(ptr: *mut u8, align: usize) -> bool {
    ptr.addr().is_multiple_of(align)
}

/// Every class size, its neighbours, and several extent sizes come back
/// non-null, aligned, and writable over the whole requested span.
#[test]
fn every_size_round_trips_with_the_full_span_written() {
    let allocator = Allocator::new();
    let sizes = CLASS_SIZES
        .iter()
        .flat_map(|&size| [size - 1, size, size + 1])
        .chain(EXTENT_SIZES);

    for (seed, size) in sizes.enumerate() {
        let layout = layout(size, 8);
        // SAFETY: the layout is valid and the block is freed below.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "size {size}");
        assert!(is_aligned(ptr, layout.align()), "size {size}");

        let block = Block {
            ptr,
            layout,
            seed: seed as u64,
        };
        block.fill();
        block.check();
        // SAFETY: allocated above with the same layout.
        unsafe { allocator.dealloc(ptr, layout) };
    }
}

/// Alignment is honoured for every size and alignment pairing, including
/// alignments above a page, and the block is writable across its span.
#[test]
fn alignment_matrix_returns_aligned_writable_blocks() {
    let allocator = Allocator::new();
    let sizes = [
        1,
        7,
        8,
        9,
        17,
        24,
        33,
        64,
        65,
        4097,
        SMALL_MAX,
        SMALL_MAX + 1,
    ];
    let aligns = [1, 2, 8, 16, 64, 128, 4096, 8192, 65536, 1 << 20];

    for size in sizes {
        for align in aligns {
            let layout = layout(size, align);
            // SAFETY: the layout is valid and the block is freed below.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null(), "size {size} align {align}");
            assert!(is_aligned(ptr, align), "size {size} align {align}");
            // SAFETY: live for `size` bytes.
            unsafe { ptr.write_bytes(0xa5, size) };
            // SAFETY: allocated above with the same layout.
            unsafe { allocator.dealloc(ptr, layout) };
        }
    }
}

/// Zero-size layouts are real, unique, aligned allocations that free
/// normally, and `realloc` to zero frees.
#[test]
fn zero_size_layouts_allocate_and_free() {
    let allocator = Allocator::new();

    for align in [1, 8, 64, 4096, 65536] {
        let layout = layout(0, align);
        // SAFETY: the layouts are valid and every block is freed below.
        unsafe {
            let first = allocator.alloc(layout);
            let second = allocator.alloc_zeroed(layout);
            assert!(!first.is_null() && !second.is_null(), "align {align}");
            assert_ne!(first, second, "align {align}");
            assert!(is_aligned(first, align) && is_aligned(second, align));
            allocator.dealloc(first, layout);
            allocator.dealloc(second, layout);
        }
    }

    let layout = layout(64, 8);
    // SAFETY: `realloc(ptr, 0)` frees `ptr`; nothing is left to free.
    unsafe {
        let ptr = allocator.alloc(layout);
        assert!(!ptr.is_null());
        assert!(allocator.realloc(ptr, layout, 0).is_null());
    }
}

/// Requests the address space cannot satisfy return null instead of
/// aborting, and a failed `realloc` leaves the original block intact.
#[test]
fn unsatisfiable_requests_return_null_and_keep_the_old_block() {
    let allocator = Allocator::new();
    let huge = Layout::from_size_align(isize::MAX as usize, 1).unwrap();

    // SAFETY: a null result is the only outcome checked.
    unsafe {
        assert!(allocator.alloc(huge).is_null());
        assert!(allocator.alloc_zeroed(huge).is_null());
    }

    let block = Block {
        // SAFETY: freed at the end of the test.
        ptr: unsafe { allocator.alloc(layout(96, 8)) },
        layout: layout(96, 8),
        seed: 7,
    };
    assert!(!block.ptr.is_null());
    block.fill();
    // SAFETY: the block stays live when `realloc` fails.
    let grown = unsafe { allocator.realloc(block.ptr, block.layout, huge.size()) };
    assert!(grown.is_null());
    block.check();
    // SAFETY: allocated above with the same layout.
    unsafe { allocator.dealloc(block.ptr, block.layout) };
}

/// `alloc_zeroed` is all zero for every class and extent size even when the
/// memory it hands out was just dirtied and freed.
#[test]
fn alloc_zeroed_is_clean_after_dirty_reuse() {
    let allocator = Allocator::new();

    for size in CLASS_SIZES.into_iter().chain(EXTENT_SIZES) {
        let layout = layout(size, 8);
        for _ in 0..3 {
            // SAFETY: the layout is valid and every block is freed.
            unsafe {
                let dirty = allocator.alloc(layout);
                assert!(!dirty.is_null(), "size {size}");
                dirty.write_bytes(0xff, size);
                allocator.dealloc(dirty, layout);

                let clean = allocator.alloc_zeroed(layout);
                assert!(!clean.is_null(), "size {size}");
                let bytes = core::slice::from_raw_parts(clean, size);
                assert!(bytes.iter().all(|&byte| byte == 0), "size {size}");
                allocator.dealloc(clean, layout);
            }
        }
    }
}

/// Growing through every class into extents and shrinking back keeps the
/// common prefix at every step. A step that stays inside one class does not
/// move the block.
#[test]
fn realloc_preserves_the_prefix_through_every_size_step() {
    let allocator = Allocator::new();

    for align in [8, 64, 4096] {
        let up = CLASS_SIZES.iter().copied().chain(EXTENT_SIZES);
        let down = EXTENT_SIZES
            .iter()
            .rev()
            .chain(CLASS_SIZES.iter().rev())
            .copied();
        let steps: Vec<usize> = [1].into_iter().chain(up).chain(down).chain([1]).collect();

        let mut block = Block {
            // SAFETY: freed at the end of the walk.
            ptr: unsafe { allocator.alloc(layout(steps[0], align)) },
            layout: layout(steps[0], align),
            seed: align as u64,
        };
        assert!(!block.ptr.is_null());
        block.fill();

        for &size in &steps[1..] {
            let old = block.layout;
            // SAFETY: `block.ptr` is live for `old`; the result replaces it.
            let moved = unsafe { allocator.realloc(block.ptr, old, size) };
            assert!(!moved.is_null(), "align {align} size {size}");
            assert!(is_aligned(moved, align), "align {align} size {size}");
            block.check_prefix(moved, old.size().min(size));

            let old_class = class_of(old.size(), align);
            if old_class.is_some() && old_class == class_of(size, align) {
                assert_eq!(moved, block.ptr, "realloc inside one class moved {size}");
            }

            block.ptr = moved;
            block.layout = layout(size, align);
            block.fill();
        }

        // SAFETY: the final pointer is live for the final layout.
        unsafe { allocator.dealloc(block.ptr, block.layout) };
    }
}

/// Pointer-only `resize` takes the new layout's alignment and keeps the
/// prefix, for a small block growing into an extent and back.
#[test]
fn resize_honours_the_new_alignment_and_keeps_the_prefix() {
    let allocator = Allocator::new();
    let mut block = Block {
        // SAFETY: freed at the end of the test.
        ptr: unsafe { allocator.alloc(layout(48, 8)) },
        layout: layout(48, 8),
        seed: 11,
    };
    assert!(!block.ptr.is_null());
    block.fill();

    for (size, align) in [(200, 64), (100 * 1024, 8192), (16, 4096), (48, 8)] {
        let new = layout(size, align);
        // SAFETY: `block.ptr` is live; the result replaces it.
        let moved = unsafe { allocator.resize(block.ptr, new) };
        assert!(!moved.is_null(), "size {size} align {align}");
        assert!(is_aligned(moved, align), "size {size} align {align}");
        block.check_prefix(moved, block.layout.size().min(size));
        block.ptr = moved;
        block.layout = new;
        block.fill();
    }

    // SAFETY: pointer-only free of the live block.
    unsafe { allocator.free(block.ptr) };
}

/// `usable_size` is at least the request, every usable byte is writable,
/// and the pointer-only `free` accepts every class and extent.
#[test]
fn usable_size_covers_a_writable_span_freed_without_a_layout() {
    let allocator = Allocator::new();
    assert_eq!(allocator.usable_size(core::ptr::null_mut()), 0);

    for size in CLASS_SIZES.into_iter().chain(EXTENT_SIZES) {
        let layout = layout(size, 16);
        // SAFETY: the layout is valid and the block is freed below.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "size {size}");

        let usable = allocator.usable_size(ptr);
        assert!(usable >= size, "size {size} usable {usable}");
        // SAFETY: the allocator reports `usable` writable bytes.
        unsafe {
            ptr.write_bytes(0x5c, usable);
            assert_eq!(ptr.add(usable - 1).read(), 0x5c);
            allocator.free(ptr);
        }
    }
}

/// Live blocks of every size never overlap, whichever order they are freed
/// in, and a second round reuses memory instead of growing without bound.
#[test]
fn live_blocks_never_overlap_and_freed_memory_is_reused() {
    const PER_SIZE: usize = 40;
    let allocator = Allocator::new();
    let mut rng = Rng::new(0x5eed);
    let sizes: Vec<usize> = CLASS_SIZES.into_iter().chain(EXTENT_SIZES).collect();

    let mut first_round = HashSet::new();
    for round in 0..2 {
        let mut live: Vec<Block> = Vec::new();
        for (seed, &size) in sizes.iter().enumerate() {
            for _ in 0..PER_SIZE {
                let layout = layout(size, 8);
                // SAFETY: the layout is valid and every block is freed below.
                let ptr = unsafe { allocator.alloc(layout) };
                assert!(!ptr.is_null(), "size {size}");
                let block = Block {
                    ptr,
                    layout,
                    seed: seed as u64,
                };
                block.fill();
                live.push(block);
            }
        }

        let mut spans: Vec<(usize, usize)> = live
            .iter()
            .map(|block| (block.ptr.addr(), block.ptr.addr() + block.layout.size()))
            .collect();
        spans.sort_unstable();
        for pair in spans.windows(2) {
            assert!(pair[0].1 <= pair[1].0, "blocks overlap: {pair:x?}");
        }

        if round == 0 {
            first_round = live.iter().map(|block| block.ptr.addr()).collect();
        } else {
            let reused = live
                .iter()
                .filter(|block| first_round.contains(&block.ptr.addr()))
                .count();
            assert!(
                reused * 2 >= live.len(),
                "only {reused} of {} blocks reused freed memory",
                live.len()
            );
        }

        while !live.is_empty() {
            let block = live.swap_remove(rng.below(live.len()));
            block.check();
            // SAFETY: allocated above with the same layout.
            unsafe { allocator.dealloc(block.ptr, block.layout) };
        }
    }
}

/// A bounded live set of one class stays inside a bounded number of runs
/// over many operations: freed blocks come back before new runs are mapped.
#[test]
fn bounded_live_set_reuses_runs() {
    const BLOCK: usize = 4096;
    const LIVE: usize = 20;
    const OPS: usize = 20_000;
    const RUN: usize = 64 * 1024;
    let max_runs = LIVE.div_ceil(RUN / BLOCK) + 1;

    let allocator = Allocator::new();
    let layout = layout(BLOCK, 8);
    let mut live = Vec::with_capacity(LIVE);
    let mut runs = HashSet::new();

    for op in 0..OPS {
        if live.is_empty() || (live.len() < LIVE && op % 3 != 0) {
            // SAFETY: the layout is valid and every block is freed below.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            runs.insert(ptr.addr() & !(RUN - 1));
            live.push(ptr);
        } else {
            let ptr = live.swap_remove(op % live.len());
            // SAFETY: allocated above with the same layout.
            unsafe { allocator.dealloc(ptr, layout) };
        }
    }

    for ptr in live {
        // SAFETY: allocated above with the same layout.
        unsafe { allocator.dealloc(ptr, layout) };
    }
    assert!(
        runs.len() <= max_runs,
        "used {} runs, max {max_runs}",
        runs.len()
    );
}
