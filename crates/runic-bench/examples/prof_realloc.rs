use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use runic::RunicAlloc;
use runic_bench::workload::realloc_sizes;

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);
    let a = RunicAlloc::new();
    let sizes = realloc_sizes();
    let start = Layout::from_size_align(1, 8).unwrap();
    for _ in 0..200 {
        let mut p = unsafe { a.alloc(start) };
        let mut cur = 1usize;
        for &size in &sizes {
            let old = Layout::from_size_align(cur, 8).unwrap();
            p = unsafe { a.realloc(p, old, size) };
            cur = size;
        }
        let old = Layout::from_size_align(cur, 8).unwrap();
        unsafe { a.dealloc(p, old) };
    }
    for _ in 0..rounds {
        let mut p = unsafe { a.alloc(black_box(start)) };
        let mut cur = 1usize;
        for &size in &sizes {
            let old = Layout::from_size_align(cur, 8).unwrap();
            p = unsafe { a.realloc(p, old, size) };
            cur = size;
        }
        let old = Layout::from_size_align(cur, 8).unwrap();
        unsafe { a.dealloc(p, old) };
    }
    core::mem::forget(a);
}
