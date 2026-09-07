use std::{alloc::Layout, hint::black_box, ptr::NonNull};

use crate::{record::AllocationRecord, rng::TraceRng, target::AllocatorTarget};

pub const SIZE_CLASSES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

pub const SINGLE_SIZE_CHURN: &[usize] = &[8, 16, 32, 64, 80, 128, 256, 512, 1024, 4096];
/// Live-set depths for freelist-heavy recycled churn (gate matrix).
pub const RECYCLED_LIVE_DEPTHS: &[usize] = &[1, 32, 256];
/// Focused local free/index hotspot sizes for profile gates (power-of-two and non-power-of-two).
///
/// `72` / `88` round into classes `80` / `96` and exercise non-power-of-two `locate`.
pub const LOCAL_HOTSPOT_SIZES: &[usize] = &[64, 72, 80, 88];
/// Phase-isolated local free/alloc probe sizes (small power-of-two, non-power-of-two, page-ish).
pub const LOCAL_PHASE_SIZES: &[usize] = &[8, 64, 80, 4096];
pub const LARGE_SIZES: &[usize] = &[32769, 64 * 1024, 256 * 1024, 1024 * 1024];
pub const ALIGNMENT_CASES: &[(usize, usize)] =
    &[(1, 8), (1, 64), (1, 4096), (64, 64), (4096, 4096)];
/// Batch size for phase-isolated owner-free / freelist-allocate benches.
pub const PHASE_BATCH: usize = 512;

/// Live slots for phase-isolated free/allocate. [`Drop`] frees only while filled.
pub struct Live {
    target: AllocatorTarget,
    layout: Layout,
    slots: Vec<NonNull<u8>>,
    filled: bool,
}

impl Live {
    fn dealloc_all(&mut self) -> usize {
        let mut checksum = 0_usize;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let ptr = *slot;
            unsafe {
                checksum ^= ptr.as_ptr().read() as usize;
                checksum ^= i;
            }
            self.target.dealloc(ptr, self.layout);
            *slot = NonNull::dangling();
        }
        checksum
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.filled {
            let _ = self.dealloc_all();
            self.filled = false;
        }
    }
}

/// Fills `count` live allocations (setup; keep outside the timed window).
///
/// # Panics
///
/// Panics if `size`/`count` are invalid or allocation fails.
#[must_use]
pub fn fill(target: AllocatorTarget, size: usize, count: usize) -> Live {
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
    Live {
        target,
        layout,
        slots,
        filled: true,
    }
}

/// Owner-free: deallocates every live slot (timed phase).
///
/// # Panics
///
/// Panics if `live` is empty.
#[must_use]
pub fn free(mut live: Live) -> usize {
    assert!(live.filled, "free requires filled slots");
    let checksum = live.dealloc_all();
    live.filled = false;
    black_box(checksum)
}

/// Seeds the freelist by freeing every live slot (setup; keep outside timed allocate).
///
/// # Panics
///
/// Panics if `live` is not filled.
pub fn seed(live: &mut Live) -> usize {
    assert!(live.filled, "seed requires filled slots");
    let checksum = live.dealloc_all();
    live.filled = false;
    black_box(checksum)
}

/// Freelist-allocate: allocates into every empty slot (timed phase).
///
/// Returns the filled [`Live`] so Criterion can drop it after the timed window.
///
/// # Panics
///
/// Panics if slots are still filled or allocation fails.
#[must_use]
pub fn allocate(mut live: Live) -> Live {
    assert!(!live.filled, "allocate requires seeded empty slots");
    let size = live.layout.size();
    let mut checksum = 0_usize;
    for (i, slot) in live.slots.iter_mut().enumerate() {
        let ptr = live.target.alloc(black_box(live.layout));
        unsafe {
            ptr.as_ptr().write(byte(i));
            ptr.as_ptr().add(size - 1).write(byte(i >> 8));
            checksum ^= ptr.as_ptr().read() as usize;
            checksum ^= ptr.as_ptr().add(size - 1).read() as usize;
        }
        *slot = ptr;
    }
    live.filled = true;
    black_box(checksum);
    live
}

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
/// # Panics
///
/// Panics if `size`/`live` are invalid or allocation fails.
#[must_use]
pub fn recycled_churn(target: AllocatorTarget, size: usize, ops: usize, live: usize) -> usize {
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
            record.check_markers();
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
        record.check_markers();
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
pub fn large_churn(target: AllocatorTarget, size: usize, ops: usize) -> usize {
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
