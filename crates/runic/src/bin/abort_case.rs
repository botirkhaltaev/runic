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
        #[cfg(any(feature = "safe", feature = "hardened"))]
        "small-double-free" => double_free(SMALL),
        #[cfg(any(feature = "safe", feature = "hardened"))]
        "large-double-free" => double_free(LARGE),
        #[cfg(any(feature = "safe", feature = "hardened"))]
        "small-remote-double-free" => remote_double_free(SMALL),
        #[cfg(any(feature = "safe", feature = "hardened"))]
        "large-remote-double-free" => remote_double_free(LARGE),
        #[cfg(feature = "hardened")]
        "smashed-link" => smashed_link(),
        #[cfg(feature = "hardened")]
        "slot-canary" => slot_canary(),
        #[cfg(feature = "hardened")]
        "extent-guard" => extent_guard(),
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

#[cfg(any(feature = "safe", feature = "hardened"))]
fn double_free(layout: Layout) {
    let ptr = allocate(layout);

    unsafe { dealloc(ptr, layout) };
    unsafe { dealloc(ptr, layout) };
}

/// Two threads that do not own the block each free it once.
#[cfg(any(feature = "safe", feature = "hardened"))]
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

/// Free one block, smash its freelist word, then free enough of the class
/// that the delay drain decodes the smashed link.
#[cfg(feature = "hardened")]
fn smashed_link() {
    const LAYOUT: Layout = Layout::new::<[u8; 64]>();
    const COUNT: usize = 4096;
    let first = allocate(LAYOUT);
    let second = allocate(LAYOUT);
    // Free both while the run still has a batch on its freelist, so they wait
    // in the delay. `second` is the tail; later frees relink that tail and
    // leave `first`'s smashed next word in place for the drain to decode.
    unsafe { dealloc(first, LAYOUT) };
    unsafe { dealloc(second, LAYOUT) };
    unsafe { first.cast::<usize>().write_unaligned(0xDEAD) };
    for _ in 0..COUNT {
        let ptr = allocate(LAYOUT);
        unsafe { dealloc(ptr, LAYOUT) };
    }
}

/// One byte in the slot's last word. The request is one word under the class,
/// so that byte is the canary, and free aborts.
#[cfg(feature = "hardened")]
fn slot_canary() {
    const LAYOUT: Layout = match Layout::from_size_align(56, 8) {
        Ok(layout) => layout,
        Err(_) => unreachable!(),
    };
    let ptr = allocate(LAYOUT);
    unsafe { ptr.add(56).write(0xFF) };
    unsafe { dealloc(ptr, LAYOUT) };
}

/// A store into the `PROT_NONE` page in front of an extent faults.
#[cfg(feature = "hardened")]
fn extent_guard() {
    let ptr = allocate(LARGE);
    unsafe { std::ptr::write_volatile(ptr.sub(1), 0xFF) };
}

fn allocate(layout: Layout) -> *mut u8 {
    let ptr = unsafe { alloc(layout) };

    if ptr.is_null() {
        std::process::exit(2);
    }

    ptr
}
