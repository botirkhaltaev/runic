//! Runs one preload case per process.
//!
//! Interposition applies at `exec` and the invalid cases end in `abort`, so
//! neither is observable from inside the test harness. Contract coverage for
//! the entry points themselves lives in the `runic-cabi` unit tests.

use core::ffi::{CStr, c_void};
use std::env;
use std::hint::black_box;
use std::thread;

const LARGE: usize = 128 * 1024;
const PAGE: usize = 4096;
const THREADS: usize = 8;
const ROUNDS: usize = 1000;

fn main() {
    let case = env::args().nth(1).expect("usage: preload-case <case>");

    match case.as_str() {
        "interposed" => interposed(),
        "threads" => threads(),
        "unknown" => {
            let mut stack = 0_usize;
            // SAFETY: a stack address never belongs to the allocator.
            unsafe { libc::free(black_box(&raw mut stack).cast()) };
        }
        // SAFETY: an interior pointer is not the base of any block.
        "small-interior" => unsafe { libc::free(interior(64, 1)) },
        "large-interior" => unsafe { libc::free(interior(LARGE, PAGE)) },
        "realloc-interior" => {
            // SAFETY: as above; `realloc` must reject it the same way.
            unsafe { libc::realloc(interior(64, 1), 128) };
        }
        other => panic!("unknown case: {other}"),
    }
}

/// Checks that the allocator serving this process is the preloaded library.
fn interposed() {
    let mut info = libc::Dl_info {
        dli_fname: core::ptr::null(),
        dli_fbase: core::ptr::null_mut(),
        dli_sname: core::ptr::null(),
        dli_saddr: core::ptr::null_mut(),
    };

    // SAFETY: `dlsym` and `dladdr` take a live name and a live output slot.
    let owner = unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"malloc".as_ptr());
        assert!(!symbol.is_null(), "malloc is unresolvable");
        assert_eq!(libc::dladdr(symbol, &raw mut info), 1, "dladdr failed");
        CStr::from_ptr(info.dli_fname)
    };

    let owner = owner.to_str().expect("non-UTF-8 library path");
    assert!(
        owner.ends_with("librunic.so"),
        "malloc resolves to {owner}, not the interceptor"
    );

    // SAFETY: the block is grown and freed through the same allocator.
    unsafe {
        let ptr = libc::malloc(64).cast::<u8>();
        assert!(!ptr.is_null(), "malloc(64) failed");
        ptr.write(0x5a);

        let grown = libc::realloc(ptr.cast(), LARGE).cast::<u8>();
        assert!(!grown.is_null(), "realloc failed");
        assert_eq!(grown.read(), 0x5a, "realloc lost the prefix");
        assert!(libc::malloc_usable_size(grown.cast()) >= LARGE);
        libc::free(grown.cast());
    }

    // Rust's own allocations reach the same place through libc.
    let mut labels: Vec<String> = (0..1024).map(|index| index.to_string()).collect();
    labels.truncate(1);
    assert_eq!(labels, ["0"]);
}

/// Exercises thread exit, where glibc registers TLS destructors via `malloc`.
fn threads() {
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            thread::spawn(|| {
                for round in 0..ROUNDS {
                    // SAFETY: each block is freed through the same allocator.
                    unsafe {
                        let ptr = libc::malloc(32 + round % 128);
                        assert!(!ptr.is_null(), "malloc failed on a worker thread");
                        libc::free(ptr);
                    }
                }
            })
        })
        .collect();

    for worker in workers {
        worker.join().expect("worker thread panicked");
    }
}

/// Allocates `size` bytes and returns a pointer `offset` bytes inside it.
///
/// `black_box` keeps the compiler from pairing up the allocation calls, the
/// way `volatile` does in equivalent C.
unsafe fn interior(size: usize, offset: usize) -> *mut c_void {
    let ptr = unsafe { libc::malloc(size) };
    assert!(!ptr.is_null(), "malloc({size}) failed");
    unsafe { black_box(ptr).byte_add(offset) }
}
