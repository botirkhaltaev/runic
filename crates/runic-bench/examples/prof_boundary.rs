use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use runic::RunicAlloc;
use runic_bench::workload::boundary_sizes;

fn main() {
    let ops: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let a = RunicAlloc::new();
    let sizes = boundary_sizes();
    for _ in 0..2_000 {
        let size = sizes[0];
        let layout = Layout::from_size_align(size, 8).unwrap();
        let p = unsafe { a.alloc(layout) };
        unsafe { a.dealloc(p, layout) };
    }
    for i in 0..ops {
        let size = sizes[i % sizes.len()];
        let layout = Layout::from_size_align(black_box(size), 8).unwrap();
        let p = unsafe { a.alloc(layout) };
        unsafe { a.dealloc(p, layout) };
    }
    core::mem::forget(a);
}
