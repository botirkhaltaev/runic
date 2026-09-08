use core::alloc::Layout;

use runic_core::Allocator;

/// Own process: run reuse under a bounded live set is process-global.
#[test]
fn allocator_reuses_runs_under_bounded_live_set() {
    const BLOCK: usize = 4096;
    const LIVE: usize = 20;
    const OPS: usize = 20_000;
    const RUN: usize = 64 * 1024;
    let capacity = RUN / BLOCK;
    let max_runs = LIVE.div_ceil(capacity) + 1;

    let allocator = Allocator::new();
    let layout = Layout::from_size_align(BLOCK, 8).unwrap();
    let mut live = Vec::with_capacity(LIVE);
    let mut bases = std::collections::HashSet::new();

    for i in 0..OPS {
        if live.is_empty() || (live.len() < LIVE && i % 3 != 0) {
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            bases.insert(ptr as usize & !(RUN - 1));
            live.push(ptr);
        } else {
            let ptr = live.swap_remove(i % live.len());
            unsafe { allocator.dealloc(ptr, layout) };
        }
    }

    for ptr in live {
        unsafe { allocator.dealloc(ptr, layout) };
    }

    assert!(
        bases.len() <= max_runs,
        "used {} runs, max {max_runs}",
        bases.len()
    );
}
