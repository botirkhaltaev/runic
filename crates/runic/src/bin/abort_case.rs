use std::{
    alloc::{Layout, alloc, dealloc, realloc},
    env,
};

use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();

fn main() {
    let Some(case) = env::args().nth(1) else {
        std::process::abort();
    };

    match case.as_str() {
        "unknown-free" => unknown_free(),
        "small-interior-free" => small_interior_free(),
        "large-interior-free" => large_interior_free(),
        "small-double-free" => small_double_free(),
        "small-interior-realloc" => small_interior_realloc(),
        "large-interior-realloc" => large_interior_realloc(),
        "small-realloc-after-free" => small_realloc_after_free(),
        _ => std::process::abort(),
    }
}

fn unknown_free() {
    let layout = Layout::from_size_align(8, 8).unwrap();
    let mut byte = 0_u8;

    unsafe { dealloc((&raw mut byte).cast::<u8>(), layout) };
}

fn small_interior_free() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = allocate(layout);

    unsafe { dealloc(ptr.add(1), layout) };
}

fn large_interior_free() {
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
    let ptr = allocate(layout);

    unsafe { dealloc(ptr.add(4096), layout) };
}

fn small_double_free() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = allocate(layout);

    unsafe { dealloc(ptr, layout) };
    unsafe { dealloc(ptr, layout) };
}

fn small_interior_realloc() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = allocate(layout);

    let _ = unsafe { realloc(ptr.add(1), layout, 128) };
}

fn large_interior_realloc() {
    let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
    let ptr = allocate(layout);

    let _ = unsafe { realloc(ptr.add(4096), layout, 256 * 1024) };
}

fn small_realloc_after_free() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let ptr = allocate(layout);

    unsafe { dealloc(ptr, layout) };
    let _ = unsafe { realloc(ptr, layout, 128) };
}

fn allocate(layout: Layout) -> *mut u8 {
    let ptr = unsafe { alloc(layout) };

    if ptr.is_null() {
        std::process::abort();
    }

    ptr
}
