use std::alloc::{Layout, dealloc};

use ferralloc::Ferralloc;

#[global_allocator]
static GLOBAL: Ferralloc = Ferralloc;

fn main() {
    let mut byte = 0_u8;
    let layout = Layout::from_size_align(1, 1).unwrap();

    unsafe { dealloc((&raw mut byte).cast(), layout) };
}
