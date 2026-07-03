use std::alloc::{Layout, alloc, dealloc};

use ferralloc::Ferralloc;

#[global_allocator]
static GLOBAL: Ferralloc = Ferralloc;

fn main() {
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
    let ptr = unsafe { alloc(layout) };

    if ptr.is_null() {
        std::process::abort();
    }

    unsafe { dealloc(ptr.add(4096), layout) };
}
