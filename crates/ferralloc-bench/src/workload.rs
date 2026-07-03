use std::{alloc::Layout, hint::black_box};

use crate::{allocation::AllocationRecord, allocator_target::AllocatorTarget, rng::TraceRng};

pub const SIZE_CLASSES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

pub const SINGLE_SIZE_CHURN: &[usize] = &[8, 16, 32, 64, 128, 256, 512, 1024, 4096];
pub const LARGE_SIZES: &[usize] = &[32769, 64 * 1024, 256 * 1024, 1024 * 1024];
pub const ALIGNMENT_CASES: &[(usize, usize)] =
    &[(1, 8), (1, 64), (1, 4096), (64, 64), (4096, 4096)];

/// Runs repeated allocate/write/free operations for one size.
///
/// # Panics
///
/// Panics if `size` cannot form a valid layout or the target allocation fails.
#[must_use]
pub fn single_size_churn(target: AllocatorTarget, size: usize, ops: usize) -> usize {
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut checksum = 0_usize;

    for i in 0..ops {
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            ptr.as_ptr().add(size - 1).write(byte(i >> 8));
            checksum ^= ptr.as_ptr().read() as usize;
            checksum ^= ptr.as_ptr().add(size - 1).read() as usize;
        }
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

/// Sweeps allocation sizes around size-class boundaries.
///
/// # Panics
///
/// Panics if a generated size cannot form a valid layout or allocation fails.
#[must_use]
pub fn size_boundary_sweep(target: AllocatorTarget, ops: usize) -> usize {
    let sizes = boundary_sizes();
    let mut checksum = 0_usize;

    for i in 0..ops {
        let size = sizes[i % sizes.len()];
        let layout = Layout::from_size_align(size, 8).unwrap();
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(size));
            ptr.as_ptr().add(size - 1).write(byte(i));
            checksum = checksum.wrapping_add(ptr.as_ptr().read() as usize);
        }
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

/// Runs a randomized small-allocation trace.
///
/// # Panics
///
/// Panics if layout construction, allocation, or pattern validation fails.
#[must_use]
pub fn small_biased_random(
    target: AllocatorTarget,
    seed: u64,
    ops: usize,
    max_live: usize,
) -> usize {
    let mut rng = TraceRng::new(seed);
    let mut live: Vec<AllocationRecord> = Vec::with_capacity(max_live);
    let mut next_id = 0_u64;
    let mut checksum = 0_usize;

    for _ in 0..ops {
        let action = rng.next_usize(100);

        if live.is_empty() || (action < 60 && live.len() < max_live) {
            let size = rng.biased_size(32 * 1024);
            let align = rng.alignment();
            let layout = Layout::from_size_align(size, align).unwrap();
            let record = if rng.next_usize(8) == 0 {
                AllocationRecord::zeroed(target, layout, next_id)
            } else {
                AllocationRecord::new(target, layout, next_id)
            };
            checksum ^= record.ptr().as_ptr() as usize;
            live.push(record);
            next_id += 1;
        } else if action < 90 {
            let index = rng.next_usize(live.len());
            let record = live.swap_remove(index);
            record.check_pattern();
            checksum ^= record.layout().size();
            record.dealloc();
        } else {
            let index = rng.next_usize(live.len());
            let new_size = rng.biased_size(32 * 1024);
            live[index].realloc(new_size);
            checksum ^= new_size;
        }
    }

    for record in live {
        record.check_pattern();
        checksum ^= record.layout().size();
        record.dealloc();
    }

    black_box(checksum)
}

/// Repeatedly allocates with a fixed size/alignment and validates alignment.
///
/// # Panics
///
/// Panics if the layout is invalid, allocation fails, or alignment is wrong.
#[must_use]
pub fn alignment_stress(target: AllocatorTarget, size: usize, align: usize, ops: usize) -> usize {
    let layout = Layout::from_size_align(size, align).unwrap();
    let mut checksum = 0_usize;

    for i in 0..ops {
        let ptr = target.alloc(black_box(layout));
        assert_eq!(ptr.as_ptr() as usize % align, 0);
        unsafe {
            ptr.as_ptr().write(byte(i));
            checksum ^= ptr.as_ptr().read() as usize;
        }
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

/// Repeatedly grows allocations through boundary sizes.
///
/// # Panics
///
/// Panics if layout construction, allocation, reallocation, or validation fails.
#[must_use]
pub fn realloc_growth(target: AllocatorTarget, rounds: usize) -> usize {
    let sizes = realloc_sizes();
    let mut checksum = 0_usize;

    for round in 0..rounds {
        let layout = Layout::from_size_align(1, 8).unwrap();
        let mut record = AllocationRecord::new(target, layout, round as u64);
        for &size in &sizes {
            record.realloc(size);
            checksum ^= record.layout().size();
        }
        record.dealloc();
    }

    black_box(checksum)
}

/// Allocates and frees a large allocation repeatedly.
///
/// # Panics
///
/// Panics if the layout is invalid, allocation fails, or alignment is wrong.
#[must_use]
pub fn large_alloc_churn(target: AllocatorTarget, size: usize, ops: usize) -> usize {
    let layout = Layout::from_size_align(size, 4096).unwrap();
    let mut checksum = 0_usize;

    for i in 0..ops {
        let ptr = target.alloc(black_box(layout));
        assert_eq!(ptr.as_ptr() as usize % 4096, 0);
        unsafe {
            ptr.as_ptr().write(byte(i));
            ptr.as_ptr().add(size - 1).write(byte(i >> 8));
            checksum ^= ptr.as_ptr().read() as usize;
        }
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

/// Allocates zeroed memory and validates marker bytes.
///
/// # Panics
///
/// Panics if the layout is invalid, allocation fails, or memory is not zeroed.
#[must_use]
pub fn alloc_zeroed(target: AllocatorTarget, size: usize, ops: usize) -> usize {
    let align = if size > 32 * 1024 { 4096 } else { 8 };
    let layout = Layout::from_size_align(size, align).unwrap();
    let mut checksum = 0_usize;

    for _ in 0..ops {
        let ptr = target.alloc_zeroed(black_box(layout));
        let first = unsafe { ptr.as_ptr().read() };
        let last = unsafe { ptr.as_ptr().add(size - 1).read() };
        assert_eq!(first, 0);
        assert_eq!(last, 0);
        checksum ^= first as usize ^ last as usize;
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

#[must_use]
pub fn boundary_sizes() -> Vec<usize> {
    let mut sizes = Vec::with_capacity(SIZE_CLASSES.len() * 3);
    for &size in SIZE_CLASSES {
        if size > 1 {
            sizes.push(size - 1);
        }
        sizes.push(size);
        sizes.push(size + 1);
    }
    sizes
}

#[must_use]
pub fn realloc_sizes() -> Vec<usize> {
    let mut sizes = Vec::new();
    for power in 0..=16 {
        let size = 1_usize << power;
        if size > 1 {
            sizes.push(size - 1);
        }
        sizes.push(size);
        sizes.push(size + 1);
    }
    sizes
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
