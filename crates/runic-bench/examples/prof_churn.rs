use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use runic::RunicAlloc;
fn main() {
    let ops: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let size: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let a = RunicAlloc::new();
    let layout = Layout::from_size_align(size, 8).unwrap();
    for _ in 0..10_000 { let p = unsafe { a.alloc(layout) }; unsafe { a.dealloc(p, layout) }; }
    for _ in 0..ops {
        let p = unsafe { a.alloc(black_box(layout)) };
        unsafe { a.dealloc(p, layout) };
    }
    core::mem::forget(a);
}
