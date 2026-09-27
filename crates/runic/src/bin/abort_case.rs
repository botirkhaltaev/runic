use std::{
    alloc::{Layout, alloc, dealloc, realloc},
    env,
};

use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();

const SMALL: Layout = Layout::new::<[u8; 64]>();
const LARGE: Layout = match Layout::from_size_align(128 * 1024, 4096) {
    Ok(layout) => layout,
    Err(_) => unreachable!(),
};

fn main() {
    let Some(case) = env::args().nth(1) else {
        std::process::exit(2);
    };

    match case.as_str() {
        "null-free" => null_free(),
        "unknown-free" => unknown_free(),
        "small-interior-free" => small_interior_free(),
        "large-interior-free" => large_interior_free(),
        "small-interior-realloc" => small_interior_realloc(),
        "large-interior-realloc" => large_interior_realloc(),
        #[cfg(feature = "safe")]
        "small-double-free" => double_free(SMALL),
        #[cfg(feature = "safe")]
        "large-double-free" => double_free(LARGE),
        #[cfg(feature = "safe")]
        "small-remote-double-free" => remote_double_free(SMALL),
        #[cfg(feature = "safe")]
        "large-remote-double-free" => remote_double_free(LARGE),
        _ => std::process::exit(2),
    }
}

fn null_free() {
    unsafe { dealloc(std::ptr::null_mut(), SMALL) };
}

fn unknown_free() {
    let mut byte = 0_u8;

    unsafe { dealloc((&raw mut byte).cast::<u8>(), SMALL) };
}

fn small_interior_free() {
    let ptr = allocate(SMALL);

    unsafe { dealloc(ptr.add(1), SMALL) };
}

fn large_interior_free() {
    let ptr = allocate(LARGE);

    unsafe { dealloc(ptr.add(4096), LARGE) };
}

fn small_interior_realloc() {
    let ptr = allocate(SMALL);

    let _ = unsafe { realloc(ptr.add(1), SMALL, 128) };
}

fn large_interior_realloc() {
    let ptr = allocate(LARGE);

    let _ = unsafe { realloc(ptr.add(4096), LARGE, 256 * 1024) };
}

#[cfg(feature = "safe")]
fn double_free(layout: Layout) {
    let ptr = allocate(layout);

    unsafe { dealloc(ptr, layout) };
    unsafe { dealloc(ptr, layout) };
}

/// Two threads that do not own the block each free it once.
#[cfg(feature = "safe")]
fn remote_double_free(layout: Layout) {
    let addr = allocate(layout).addr();

    for _ in 0..2 {
        std::thread::spawn(move || unsafe {
            dealloc(std::ptr::with_exposed_provenance_mut(addr), layout);
        })
        .join()
        .unwrap();
    }
}

fn allocate(layout: Layout) -> *mut u8 {
    let ptr = unsafe { alloc(layout) };

    if ptr.is_null() {
        std::process::exit(2);
    }

    ptr
}
