use core::alloc::Layout;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use runic_core::Allocator;

const CLASS_SIZES: &[usize] = &[
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

#[test]
fn allocator_reallocates_after_freeing_small_block() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();

    let first = unsafe { allocator.alloc(layout) };
    assert!(!first.is_null());
    let second = unsafe { allocator.alloc(layout) };
    assert!(!second.is_null());

    unsafe { allocator.dealloc(second, layout) };

    let reused = unsafe { allocator.alloc(layout) };
    assert!(!reused.is_null());

    unsafe { allocator.dealloc(reused, layout) };
    unsafe { allocator.dealloc(first, layout) };
}

#[test]
fn allocator_returns_aligned_pointer() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(1, 4096).unwrap();

    let ptr = unsafe { allocator.alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % layout.align(), 0);

    unsafe { allocator.dealloc(ptr, layout) };
}

#[test]
fn allocator_zeroes_memory() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(256, 8).unwrap();

    let ptr = unsafe { allocator.alloc_zeroed(layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));

    unsafe { allocator.dealloc(ptr, layout) };
}

#[test]
fn allocator_zeroes_memory_after_local_small_fast_path_is_ready() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();

    let seed = unsafe { allocator.alloc(layout) };
    assert!(!seed.is_null());

    let ptr = unsafe { allocator.alloc_zeroed(layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));

    unsafe { allocator.dealloc(ptr, layout) };
    unsafe { allocator.dealloc(seed, layout) };
}

#[test]
fn allocator_realloc_preserves_prefix() {
    let allocator = Allocator::new();
    let old = Layout::from_size_align(32, 8).unwrap();
    let ptr = unsafe { allocator.alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write(byte(index)) };
    }

    let new_ptr = unsafe { allocator.realloc(ptr, old, 128) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, byte(index));
    }

    let new = Layout::from_size_align(128, 8).unwrap();
    unsafe { allocator.dealloc(new_ptr, new) };
}

#[test]
fn allocator_realloc_uses_old_layout_size_for_copy_len() {
    let allocator = Allocator::new();
    let old = Layout::from_size_align(17, 8).unwrap();
    let ptr = unsafe { allocator.alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write(byte(index + 1)) };
    }

    let new_ptr = unsafe { allocator.realloc(ptr, old, 128) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, byte(index + 1));
    }

    let new = Layout::from_size_align(128, 8).unwrap();
    unsafe { allocator.dealloc(new_ptr, new) };
}

#[test]
fn allocator_handles_large_allocation() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();

    let ptr = unsafe { allocator.alloc(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % layout.align(), 0);

    unsafe {
        ptr.write(0xab);
        ptr.add(layout.size() - 1).write(0xcd);
    }

    unsafe { allocator.dealloc(ptr, layout) };
}

#[test]
fn allocator_allocates_and_frees_each_size_class() {
    let allocator = Allocator::new();
    let mut allocations = Vec::new();

    for &size in CLASS_SIZES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        let ptr = unsafe { allocator.alloc(layout) };

        assert!(!ptr.is_null(), "size {size}");
        unsafe {
            ptr.write(0xab);
            ptr.add(size - 1).write(0xcd);
        }
        allocations.push((ptr, layout));
    }

    for (ptr, layout) in allocations {
        unsafe { allocator.dealloc(ptr, layout) };
    }
}

#[test]
fn allocator_returns_aligned_pointer_for_size_alignment_matrix() {
    let allocator = Allocator::new();
    let sizes = [
        1, 7, 8, 9, 15, 16, 17, 23, 24, 25, 31, 32, 33, 63, 64, 65, 4097,
    ];
    let aligns = [1, 2, 4, 8, 16, 32, 64, 128, 4096, 65536];

    for size in sizes {
        for align in aligns {
            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = unsafe { allocator.alloc(layout) };

            assert!(!ptr.is_null(), "size {size}, align {align}");
            assert_eq!(ptr as usize % align, 0, "size {size}, align {align}");

            unsafe { allocator.dealloc(ptr, layout) };
        }
    }
}

#[test]
fn allocator_handles_many_small_allocations_across_run_boundary() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(8, 8).unwrap();
    let mut allocations = Vec::new();

    for index in 0_usize..9000 {
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "index {index}");
        unsafe { ptr.write(byte(index)) };
        allocations.push((ptr, index));
    }

    for (ptr, index) in &allocations {
        assert_eq!(unsafe { ptr.read() }, byte(*index));
    }

    for (ptr, _) in allocations {
        unsafe { allocator.dealloc(ptr, layout) };
    }
}

#[test]
fn allocator_realloc_small_to_large_preserves_prefix() {
    let allocator = Allocator::new();
    let old = Layout::from_size_align(1024, 16).unwrap();
    let ptr = unsafe { allocator.alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..old.size() {
        unsafe { ptr.add(index).write(byte(index)) };
    }

    let new_ptr = unsafe { allocator.realloc(ptr, old, 128 * 1024) };
    assert!(!new_ptr.is_null());

    for index in 0..old.size() {
        assert_eq!(unsafe { new_ptr.add(index).read() }, byte(index));
    }

    let new = Layout::from_size_align(128 * 1024, 16).unwrap();
    unsafe { allocator.dealloc(new_ptr, new) };
}

#[test]
fn allocator_realloc_large_to_small_preserves_prefix() {
    let allocator = Allocator::new();
    let old = Layout::from_size_align(128 * 1024, 64).unwrap();
    let ptr = unsafe { allocator.alloc(old) };
    assert!(!ptr.is_null());

    for index in 0..4096 {
        unsafe { ptr.add(index).write(byte(index)) };
    }

    let new_ptr = unsafe { allocator.realloc(ptr, old, 4096) };
    assert!(!new_ptr.is_null());

    for index in 0..4096 {
        assert_eq!(unsafe { new_ptr.add(index).read() }, byte(index));
    }

    let new = Layout::from_size_align(4096, 64).unwrap();
    unsafe { allocator.dealloc(new_ptr, new) };
}

#[test]
fn allocator_zeroes_large_memory() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(96 * 1024, 4096).unwrap();
    let ptr = unsafe { allocator.alloc_zeroed(layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));

    unsafe { allocator.dealloc(ptr, layout) };
}

#[test]
fn allocator_survives_deterministic_random_trace() {
    const OPS: usize = 10_000;
    const MAX_LIVE: usize = 512;

    let allocator = Allocator::new();
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
                unsafe { allocator.alloc_zeroed(layout) }
            } else {
                unsafe { allocator.alloc(layout) }
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
            unsafe { allocator.dealloc(record.ptr, record.layout) };
        } else {
            let index = rng.next_usize(live.len());
            live[index].check_pattern();

            let new_size = rng.biased_size(64 * 1024);
            let old = live[index].layout;
            let new_ptr = unsafe { allocator.realloc(live[index].ptr, old, new_size) };
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
        unsafe { allocator.dealloc(record.ptr, record.layout) };
    }
}

#[test]
fn allocator_cold_switches_between_instances_in_one_thread() {
    let first = Allocator::new();
    let second = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();

    let first_ptr = unsafe { first.alloc(layout) };
    let second_ptr = unsafe { second.alloc(layout) };
    let first_again = unsafe { first.alloc(layout) };

    assert!(!first_ptr.is_null());
    assert!(!second_ptr.is_null());
    assert!(!first_again.is_null());

    unsafe { first.dealloc(first_ptr, layout) };
    unsafe { second.dealloc(second_ptr, layout) };
    unsafe { first.dealloc(first_again, layout) };
}

#[test]
fn allocator_drop_before_thread_local_teardown_is_safe() {
    let layout = Layout::from_size_align(64, 8).unwrap();

    let thread = thread::spawn(move || {
        let allocator = Allocator::new();
        let ptr = unsafe { allocator.alloc(layout) };

        assert!(!ptr.is_null());

        unsafe { allocator.dealloc(ptr, layout) };
        drop(allocator);
    });

    thread.join().unwrap();
}

#[test]
fn allocator_supports_scoped_threaded_use() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(128, 8).unwrap();

    thread::scope(|scope| {
        for byte in 0_u8..4 {
            let allocator = &allocator;
            scope.spawn(move || {
                let ptr = unsafe { allocator.alloc(layout) };

                assert!(!ptr.is_null());

                unsafe {
                    ptr.write(byte);
                    assert_eq!(ptr.read(), byte);
                    allocator.dealloc(ptr, layout);
                }
            });
        }
    });
}

#[test]
fn allocator_frees_thread_owned_small_allocation_after_owner_thread_exits() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();

    let ptr = thread::scope(|scope| {
        let allocator = &allocator;
        scope
            .spawn(move || {
                let ptr = unsafe { allocator.alloc(layout) };
                assert!(!ptr.is_null());
                unsafe { ptr.write(0x5a) };
                ptr.addr()
            })
            .join()
            .unwrap()
    });

    let ptr = ptr as *mut u8;
    assert_eq!(unsafe { ptr.read() }, 0x5a);
    unsafe { allocator.dealloc(ptr, layout) };
}

#[test]
fn allocator_drains_remote_free_when_owner_thread_exits() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();
    let (ptr_tx, ptr_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    thread::scope(|scope| {
        let allocator = &allocator;
        scope.spawn(move || {
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            ptr_tx.send(ptr.addr()).unwrap();
            done_rx.recv().unwrap();
        });

        let ptr = ptr_rx.recv().unwrap() as *mut u8;
        unsafe { allocator.dealloc(ptr, layout) };
        done_tx.send(()).unwrap();
    });
}

#[test]
fn allocator_publishes_retained_remote_batch_after_owner_exits() {
    // Freer claims while owner is Active (TLS batch retains below capacity), owner then
    // exits to Draining, and freer TLS teardown must complete the batch without abort.
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();
    let (ptr_tx, ptr_rx) = mpsc::channel();
    let (batched_tx, batched_rx) = mpsc::channel();
    let (owner_done_tx, owner_done_rx) = mpsc::channel();

    thread::scope(|scope| {
        let allocator = &allocator;
        scope.spawn(move || {
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            ptr_tx.send(ptr.addr()).unwrap();
            batched_rx.recv().unwrap();
            owner_done_tx.send(()).unwrap();
        });

        scope.spawn(move || {
            let ptr = ptr_rx.recv().unwrap() as *mut u8;
            unsafe { allocator.dealloc(ptr, layout) };
            batched_tx.send(()).unwrap();
            owner_done_rx.recv().unwrap();
        });
    });
}

#[test]
fn allocator_frees_large_allocation_from_non_owner_thread() {
    let allocator = Allocator::new();
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
    let (ptr_tx, ptr_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    thread::scope(|scope| {
        let allocator = &allocator;
        scope.spawn(move || {
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            ptr_tx.send(ptr.addr()).unwrap();
            done_rx.recv().unwrap();
        });

        let ptr = ptr_rx.recv().unwrap() as *mut u8;
        unsafe { allocator.dealloc(ptr, layout) };
        done_tx.send(()).unwrap();
    });
}

#[test]
fn allocator_completes_remote_free_burst_without_owner_progress() {
    // Regression test for the remote-free livelock (#61): a burst of remote
    // frees far larger than any internal remote-free queue capacity must
    // return without deadlock even while the owner thread makes no allocator
    // progress of its own.
    const BURST: usize = 4 * 1024;
    const WATCHDOG_SECONDS: u64 = 60;

    let allocator = Allocator::new();
    let layout = Layout::from_size_align(64, 8).unwrap();
    let (ptr_tx, ptr_rx) = mpsc::channel();
    let (park_tx, park_rx) = mpsc::channel();
    let disarmed = Arc::new(AtomicBool::new(false));

    // A deadlock in the free path must abort the test process quickly instead
    // of hanging the suite until the CI job timeout (see issue #60).
    thread::spawn({
        let disarmed = Arc::clone(&disarmed);
        move || {
            thread::sleep(Duration::from_secs(WATCHDOG_SECONDS));
            if !disarmed.load(Ordering::Acquire) {
                std::process::abort();
            }
        }
    });

    thread::scope(|scope| {
        let allocator = &allocator;
        let owner = scope.spawn(move || {
            for _ in 0..BURST {
                let ptr = unsafe { allocator.alloc(layout) };
                assert!(!ptr.is_null());
                unsafe { ptr.write(0x5a) };
                ptr_tx.send(ptr.addr()).unwrap();
            }
            // Stay parked so every free below arrives remotely while the
            // owner performs no local allocation or free of its own.
            park_rx.recv().unwrap();
        });

        let mut pointers = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            pointers.push(ptr_rx.recv().unwrap() as *mut u8);
        }

        for ptr in pointers {
            assert_eq!(unsafe { ptr.read() }, 0x5a);
            unsafe { allocator.dealloc(ptr, layout) };
        }

        // The allocator must still make forward progress after the burst.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { ptr.write(0xa5) };
        assert_eq!(unsafe { ptr.read() }, 0xa5);
        unsafe { allocator.dealloc(ptr, layout) };

        park_tx.send(()).unwrap();
        owner.join().unwrap();
    });

    disarmed.store(true, Ordering::Release);
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
        let value = self
            .id
            .wrapping_mul(131)
            .wrapping_add(u64::try_from(index).unwrap())
            .wrapping_add(u64::try_from(self.layout.size()).unwrap());

        value.to_le_bytes()[0]
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
        let upper = u64::try_from(upper).unwrap();
        let value = self.next() % upper;

        usize::try_from(value).unwrap()
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

fn byte(value: usize) -> u8 {
    u8::try_from(value % 251).unwrap()
}
