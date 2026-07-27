use std::{alloc::Layout, hint::black_box, ptr::NonNull};

use crate::{allocation::AllocationRecord, allocator_target::AllocatorTarget, rng::TraceRng};

pub const SIZE_CLASSES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

pub const SINGLE_SIZE_CHURN: &[usize] = &[8, 16, 32, 64, 80, 128, 256, 512, 1024, 4096];
/// Live-set depths for freelist-heavy recycled churn (gate matrix).
pub const RECYCLED_LIVE_DEPTHS: &[usize] = &[1, 32, 256];
/// Focused local free/index hotspot sizes for profile gates (power-of-two and non-power-of-two requests).
///
/// `72` / `88` round into classes `80` / `96` and exercise non-power-of-two `locate`.
pub const LOCAL_HOTSPOT_SIZES: &[usize] = &[64, 72, 80, 88];
/// Phase-isolated local free/alloc probe sizes (small power-of-two, sticky, non-power-of-two, page-ish).
pub const LOCAL_PHASE_SIZES: &[usize] = &[8, 64, 80, 4096];
pub const LARGE_SIZES: &[usize] = &[32769, 64 * 1024, 256 * 1024, 1024 * 1024];
pub const ALIGNMENT_CASES: &[(usize, usize)] =
    &[(1, 8), (1, 64), (1, 4096), (64, 64), (4096, 4096)];
/// Batch size for phase-isolated owner-free / freelist-allocate benches.
pub const PHASE_BATCH: usize = 512;

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

/// Recycled live-set churn: keep `live` allocations and replace them round-robin.
///
/// Depth 1 matches immediate free/reuse; deeper live sets exercise freelist allocate
/// after owner free without collapsing to bump-only traffic.
///
/// # Panics
///
/// Panics if `size`/`live` are invalid or allocation fails.
#[must_use]
pub fn single_size_recycled_churn(
    target: AllocatorTarget,
    size: usize,
    ops: usize,
    live: usize,
) -> usize {
    assert!(live > 0, "live depth must be non-zero");
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut slots = Vec::with_capacity(live);
    let mut checksum = 0_usize;

    for i in 0..live {
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            checksum ^= ptr.as_ptr().read() as usize;
        }
        slots.push(ptr);
    }

    for i in 0..ops {
        let index = i % live;
        let old = slots[index];
        target.dealloc(old, layout);
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            ptr.as_ptr().add(size - 1).write(byte(i >> 8));
            checksum ^= ptr.as_ptr().read() as usize;
            checksum ^= ptr.as_ptr().add(size - 1).read() as usize;
        }
        slots[index] = ptr;
    }

    for ptr in slots {
        target.dealloc(ptr, layout);
    }

    black_box(checksum)
}

/// Fills `count` live allocations (setup; keep outside the timed window).
///
/// # Panics
///
/// Panics if `size`/`count` are invalid or allocation fails.
#[must_use]
pub fn fill_live(target: AllocatorTarget, size: usize, count: usize) -> Vec<NonNull<u8>> {
    assert!(count > 0, "live count must be non-zero");
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut slots = Vec::with_capacity(count);
    for i in 0..count {
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
        }
        slots.push(ptr);
    }
    slots
}

/// Owner-free-only: deallocates every live slot (timed phase).
///
/// Caller must refill via [`fill_live`] or [`refill_live`] before the next free phase.
///
/// # Panics
///
/// Panics if `size` is invalid.
#[must_use]
pub fn owner_free_only(target: AllocatorTarget, size: usize, slots: &mut [NonNull<u8>]) -> usize {
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut checksum = 0_usize;
    for (i, slot) in slots.iter_mut().enumerate() {
        let ptr = *slot;
        unsafe {
            checksum ^= ptr.as_ptr().read() as usize;
            checksum ^= i;
        }
        target.dealloc(ptr, layout);
        // Leave a dangling placeholder; refill restores valid pointers.
        *slot = NonNull::dangling();
    }
    black_box(checksum)
}

/// Allocates into every slot (setup refill after [`owner_free_only`]).
///
/// # Panics
///
/// Panics if `size` is invalid or allocation fails.
#[must_use]
pub fn refill_live(target: AllocatorTarget, size: usize, slots: &mut [NonNull<u8>]) -> usize {
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut checksum = 0_usize;
    for (i, slot) in slots.iter_mut().enumerate() {
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            checksum ^= ptr.as_ptr().read() as usize;
        }
        *slot = ptr;
    }
    black_box(checksum)
}

/// Seeds the freelist by freeing every live slot (setup; keep outside timed allocate).
///
/// # Panics
///
/// Panics if `size` is invalid.
#[must_use]
pub fn seed_freelist(target: AllocatorTarget, size: usize, slots: &mut [NonNull<u8>]) -> usize {
    owner_free_only(target, size, slots)
}

/// Freelist-allocate-only: allocates into every slot from a seeded freelist (timed phase).
///
/// Call [`fill_live`] then [`seed_freelist`] before the first timed call. After timing,
/// either free the slots for cleanup or re-seed for the next sample.
///
/// # Panics
///
/// Panics if `size` is invalid or allocation fails.
#[must_use]
pub fn freelist_allocate_only(
    target: AllocatorTarget,
    size: usize,
    slots: &mut [NonNull<u8>],
) -> usize {
    let layout = Layout::from_size_align(size, 8).unwrap();
    let mut checksum = 0_usize;
    for (i, slot) in slots.iter_mut().enumerate() {
        let ptr = target.alloc(black_box(layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            checksum ^= ptr.as_ptr().read() as usize;
            checksum ^= ptr.as_ptr().add(size - 1).read() as usize;
        }
        *slot = ptr;
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

/// Allocates and frees large allocations from Runic's large-size benchmark set.
///
/// # Panics
///
/// Panics if layout construction, allocation, alignment, or validation fails.
#[must_use]
pub fn mixed_large_churn(target: AllocatorTarget, ops: usize) -> usize {
    let mut checksum = 0_usize;

    for i in 0..ops {
        let size = LARGE_SIZES[i % LARGE_SIZES.len()];
        let layout = Layout::from_size_align(size, 4096).unwrap();
        let ptr = target.alloc(black_box(layout));
        assert_eq!(ptr.as_ptr() as usize % 4096, 0);
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
