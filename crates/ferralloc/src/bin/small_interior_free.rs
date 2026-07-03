use std::alloc::{Layout, alloc, dealloc};

use ferralloc::Ferralloc;

#[global_allocator]
static GLOBAL: Ferralloc = Ferralloc;

fn main() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = unsafe { alloc(layout) };

    if ptr.is_null() {
        std::process::abort();
    }

    unsafe { dealloc(ptr.add(1), layout) };
}
