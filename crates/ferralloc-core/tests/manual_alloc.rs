use core::alloc::Layout;

use ferralloc_core::Allocator;

const CLASS_SIZES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

#[test]
fn allocator_reuses_freed_small_block() {
    let layout = Layout::from_size_align(64, 8).unwrap();

    let first = unsafe { Allocator::alloc(layout) };
    assert!(!first.is_null());

    unsafe { Allocator::dealloc(first, layout) };

    let second = unsafe { Allocator::alloc(layout) };
    assert_eq!(first, second);

    unsafe { Allocator::dealloc(second, layout) };
}

#[test]
fn allocator_returns_aligned_pointer() {
    let layout = Layout::from_size_align(1, 4096).unwrap();

    let ptr = unsafe { Allocator::alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % layout.align(), 0);

    unsafe { Allocator::dealloc(ptr, layout) };
}

#[test]
fn allocator_zeroes_memory() {
    let layout = Layout::from_size_align(256, 8).unwrap();

    let ptr = unsafe { Allocator::alloc_zeroed(layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));

    unsafe { Allocator::dealloc(ptr, layout) };
}

#[test]
fn allocator_realloc_preserves_prefix() {
    let old = Layout::from_size_align(32, 8).unwrap();
    let ptr = unsafe { Allocator::alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write(index as u8) };
    }

    let new_ptr = unsafe { Allocator::realloc(ptr, old, 128) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, index as u8);
    }

    let new = Layout::from_size_align(128, 8).unwrap();
    unsafe { Allocator::dealloc(new_ptr, new) };
}

#[test]
fn allocator_realloc_uses_old_layout_size_for_copy_len() {
    let old = Layout::from_size_align(17, 8).unwrap();
    let ptr = unsafe { Allocator::alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write((index + 1) as u8) };
    }

    let new_ptr = unsafe { Allocator::realloc(ptr, old, 128) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, (index + 1) as u8);
    }

    let new = Layout::from_size_align(128, 8).unwrap();
    unsafe { Allocator::dealloc(new_ptr, new) };
}

#[test]
fn allocator_handles_large_allocation() {
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();

    let ptr = unsafe { Allocator::alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % layout.align(), 0);

    unsafe {
        ptr.write(0xab);
        ptr.add(layout.size() - 1).write(0xcd);
    }

    unsafe { Allocator::dealloc(ptr, layout) };
}

#[test]
fn allocator_allocates_and_frees_each_size_class() {
    let mut allocations = Vec::new();

    for &size in CLASS_SIZES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        let ptr = unsafe { Allocator::alloc(layout) };

        assert!(!ptr.is_null(), "size {size}");
        unsafe {
            ptr.write(0xab);
            ptr.add(size - 1).write(0xcd);
        }
        allocations.push((ptr, layout));
    }

    for (ptr, layout) in allocations {
        unsafe { Allocator::dealloc(ptr, layout) };
    }
}

#[test]
fn allocator_returns_aligned_pointer_for_size_alignment_matrix() {
    let sizes = [
        1, 7, 8, 9, 15, 16, 17, 23, 24, 25, 31, 32, 33, 63, 64, 65, 4097,
    ];
    let aligns = [1, 2, 4, 8, 16, 32, 64, 128, 4096, 65536];

    for size in sizes {
        for align in aligns {
            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = unsafe { Allocator::alloc(layout) };

            assert!(!ptr.is_null(), "size {size}, align {align}");
            assert_eq!(ptr as usize % align, 0, "size {size}, align {align}");

            unsafe { Allocator::dealloc(ptr, layout) };
        }
    }
}

#[test]
fn allocator_handles_many_small_allocations_across_span_boundary() {
    let layout = Layout::from_size_align(8, 8).unwrap();
    let mut allocations = Vec::new();

    for index in 0..9000 {
        let ptr = unsafe { Allocator::alloc(layout) };
        assert!(!ptr.is_null(), "index {index}");
        unsafe { ptr.write((index % 251) as u8) };
        allocations.push((ptr, index));
    }

    for (ptr, index) in &allocations {
        assert_eq!(unsafe { ptr.read() }, (index % 251) as u8);
    }

    for (ptr, _) in allocations {
        unsafe { Allocator::dealloc(ptr, layout) };
    }
}

#[test]
fn allocator_realloc_small_to_large_preserves_prefix() {
    let old = Layout::from_size_align(1024, 16).unwrap();
    let ptr = unsafe { Allocator::alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write((index % 251) as u8) };
    }

    let new_ptr = unsafe { Allocator::realloc(ptr, old, 128 * 1024) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, (index % 251) as u8);
    }

    let new = Layout::from_size_align(128 * 1024, 16).unwrap();
    unsafe { Allocator::dealloc(new_ptr, new) };
}

#[test]
fn allocator_realloc_large_to_small_preserves_prefix() {
    let old = Layout::from_size_align(128 * 1024, 64).unwrap();
    let ptr = unsafe { Allocator::alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..4096 {
        unsafe { ptr.add(index).write((index % 251) as u8) };
    }

    let new_ptr = unsafe { Allocator::realloc(ptr, old, 4096) };
    assert!(!new_ptr.is_null());

    for index in 0..4096 {
        assert_eq!(unsafe { new_ptr.add(index).read() }, (index % 251) as u8);
    }

    let new = Layout::from_size_align(4096, 64).unwrap();
    unsafe { Allocator::dealloc(new_ptr, new) };
}

#[test]
fn allocator_zeroes_large_memory() {
    let layout = Layout::from_size_align(96 * 1024, 4096).unwrap();
    let ptr = unsafe { Allocator::alloc_zeroed(layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));

    unsafe { Allocator::dealloc(ptr, layout) };
}

#[test]
fn allocator_survives_deterministic_random_trace() {
    const OPS: usize = 10_000;
    const MAX_LIVE: usize = 512;

    let mut rng = TraceRng::new(0xf3ee_a110_c001_cafe);
    let mut live = Vec::new();
    let mut next_id = 0_u64;

    for _ in 0..OPS {
        let action = rng.next_usize(100);

        if live.is_empty() || (action < 60 && live.len() < MAX_LIVE) {
            let size = rng.biased_size(64 * 1024);
            let align = rng.alignment();
            let layout = Layout::from_size_align(size, align).unwrap();
            let zeroed = rng.next_usize(4) == 0;
            let ptr = if zeroed {
                unsafe { Allocator::alloc_zeroed(layout) }
            } else {
                unsafe { Allocator::alloc(layout) }
            };

            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % align, 0);

            if zeroed {
                let bytes = unsafe { core::slice::from_raw_parts(ptr, size) };
                assert!(bytes.iter().all(|&byte| byte == 0));
            }

            let record = AllocationRecord {
                ptr,
                layout,
                id: next_id,
            };
            record.write_pattern();
            live.push(record);
            next_id += 1;
        } else if action < 90 {
            let index = rng.next_usize(live.len());
            let record = live.swap_remove(index);
            record.check_pattern();
            unsafe { Allocator::dealloc(record.ptr, record.layout) };
        } else {
            let index = rng.next_usize(live.len());
            live[index].check_pattern();

            let new_size = rng.biased_size(64 * 1024);
            let old = live[index].layout;
            let new_ptr = unsafe { Allocator::realloc(live[index].ptr, old, new_size) };
            assert!(!new_ptr.is_null());
            assert_eq!(new_ptr as usize % old.align(), 0);

            let preserved = old.size().min(new_size);
            live[index].check_prefix(new_ptr, preserved);

            live[index] = AllocationRecord {
                ptr: new_ptr,
                layout: Layout::from_size_align(new_size, old.align()).unwrap(),
                id: live[index].id,
            };
            live[index].write_pattern();
        }
    }

    for record in live {
        record.check_pattern();
        unsafe { Allocator::dealloc(record.ptr, record.layout) };
    }
}

struct AllocationRecord {
    ptr: *mut u8,
    layout: Layout,
    id: u64,
}

impl AllocationRecord {
    fn write_pattern(&self) {
        for index in 0..self.layout.size() {
            unsafe { self.ptr.add(index).write(self.byte_at(index)) };
        }
    }

    fn check_pattern(&self) {
        self.check_prefix(self.ptr, self.layout.size());
    }

    fn check_prefix(&self, ptr: *mut u8, len: usize) {
        for index in 0..len {
            assert_eq!(unsafe { ptr.add(index).read() }, self.byte_at(index));
        }
    }

    fn byte_at(&self, index: usize) -> u8 {
        self.id
            .wrapping_mul(131)
            .wrapping_add(index as u64)
            .wrapping_add(self.layout.size() as u64) as u8
    }
}

struct TraceRng(u64);

impl TraceRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn next_usize(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn biased_size(&mut self, max: usize) -> usize {
        let cap = self.next_usize(max).max(16);
        self.next_usize(cap).max(1)
    }

    fn alignment(&mut self) -> usize {
        const ALIGNS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128, 4096];

        ALIGNS[self.next_usize(ALIGNS.len())]
    }
}
