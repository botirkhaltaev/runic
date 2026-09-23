//! C malloc-family entry points for `LD_PRELOAD` (`librunic.so`).
//!
//! Entry points map C arguments to [`runic_core::Allocator`] and return C errno
//! or null. C `free` carries no layout and accepts null.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("runic-cabi supports Linux x86_64 only");

use core::alloc::Layout;
use core::ffi::{c_int, c_void};
use core::ptr::null_mut;

use runic_core::Allocator;

/// `max_align_t` on Linux `x86_64`.
const MAX_ALIGN: usize = 16;
const PAGE: usize = 4096;

static ALLOC: Allocator = Allocator::new();

#[cold]
fn errno_null(code: c_int) -> *mut c_void {
    // SAFETY: `__errno_location` returns this thread's live errno slot.
    unsafe { *libc::__errno_location() = code };
    null_mut()
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let Ok(layout) = Layout::from_size_align(size, MAX_ALIGN) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `layout` is well-formed.
    let ptr: *mut c_void = unsafe { ALLOC.alloc(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the caller supplies a live allocation from this allocator.
    unsafe { ALLOC.free(ptr.cast()) };
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let Some(total) = nmemb.checked_mul(size) else {
        return errno_null(libc::ENOMEM);
    };
    let Ok(layout) = Layout::from_size_align(total, MAX_ALIGN) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `layout` is well-formed.
    let ptr: *mut c_void = unsafe { ALLOC.alloc_zeroed(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        // SAFETY: `realloc(NULL, n)` is `malloc(n)`.
        return unsafe { malloc(size) };
    }
    if size == 0 {
        // SAFETY: `ptr` is non-null and live.
        unsafe { free(ptr) };
        return null_mut();
    }
    let Ok(new) = Layout::from_size_align(size, MAX_ALIGN) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `ptr` is live and `new` is well-formed.
    let resized: *mut c_void = unsafe { ALLOC.resize(ptr.cast(), new) }.cast();
    if resized.is_null() {
        return errno_null(libc::ENOMEM);
    }
    resized
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    if memptr.is_null() || alignment < size_of::<*mut c_void>() || !alignment.is_power_of_two() {
        return libc::EINVAL;
    }
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return libc::ENOMEM;
    };
    // SAFETY: `layout` is well-formed.
    let ptr = unsafe { ALLOC.alloc(layout) };
    if ptr.is_null() {
        return libc::ENOMEM;
    }
    // SAFETY: `memptr` is a live output slot.
    unsafe { memptr.write(ptr.cast()) };
    0
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() || !size.is_multiple_of(alignment) {
        return errno_null(libc::EINVAL);
    }
    // SAFETY: the C11 size constraint was checked above.
    unsafe { memalign(alignment, size) }
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() {
        return errno_null(libc::EINVAL);
    }
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `layout` is well-formed.
    let ptr: *mut c_void = unsafe { ALLOC.alloc(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
    // SAFETY: `PAGE` is a power of two.
    unsafe { memalign(PAGE, size) }
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn pvalloc(size: usize) -> *mut c_void {
    let Some(rounded) = size
        .max(1)
        .checked_add(PAGE - 1)
        .map(|len| len & !(PAGE - 1))
    else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `valloc` accepts any size.
    unsafe { valloc(rounded) }
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn reallocarray(ptr: *mut c_void, nmemb: usize, size: usize) -> *mut c_void {
    let Some(total) = nmemb.checked_mul(size) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: same contract as `realloc`.
    unsafe { realloc(ptr, total) }
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn cfree(ptr: *mut c_void) {
    // SAFETY: same contract as `free`.
    unsafe { free(ptr) };
}

#[cfg_attr(not(test), unsafe(no_mangle))]
unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> usize {
    ALLOC.usable_size(ptr.cast())
}

mod glibc {
    use core::ffi::c_void;

    macro_rules! forward {
        ($name:ident => $target:ident($($arg:ident: $ty:ty),*) -> $ret:ty) => {
            #[cfg_attr(not(test), unsafe(no_mangle))]
            unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
                // SAFETY: same contract as the public entry point.
                unsafe { super::$target($($arg),*) }
            }
        };
        ($name:ident => $target:ident($($arg:ident: $ty:ty),*)) => {
            #[cfg_attr(not(test), unsafe(no_mangle))]
            unsafe extern "C" fn $name($($arg: $ty),*) {
                // SAFETY: same contract as the public entry point.
                unsafe { super::$target($($arg),*) };
            }
        };
    }

    forward!(__libc_malloc => malloc(size: usize) -> *mut c_void);
    forward!(__libc_free => free(ptr: *mut c_void));
    forward!(__libc_calloc => calloc(nmemb: usize, size: usize) -> *mut c_void);
    forward!(__libc_realloc => realloc(ptr: *mut c_void, size: usize) -> *mut c_void);
    forward!(__libc_memalign => memalign(alignment: usize, size: usize) -> *mut c_void);
    forward!(__libc_valloc => valloc(size: usize) -> *mut c_void);
    forward!(__libc_pvalloc => pvalloc(size: usize) -> *mut c_void);
    forward!(__libc_cfree => cfree(ptr: *mut c_void));
    forward!(
        __libc_posix_memalign =>
        posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_zero_size_follow_c_contracts() {
        // SAFETY: C `free(NULL)` is a no-op; zero-size results are freeable.
        unsafe {
            free(null_mut());
            let ptr = malloc(0);
            assert!(!ptr.is_null());
            free(ptr);
            let ptr = realloc(null_mut(), 0);
            assert!(!ptr.is_null());
            free(ptr);
        }
        // SAFETY: null is explicitly accepted.
        assert_eq!(unsafe { malloc_usable_size(null_mut()) }, 0);
    }

    #[test]
    fn malloc_realloc_free_preserves_prefix() {
        // SAFETY: matching malloc, realloc, and free.
        unsafe {
            let ptr = malloc(64);
            assert!(!ptr.is_null());
            ptr.cast::<u8>().write(0x5a);
            let grown = realloc(ptr, 128);
            assert!(!grown.is_null());
            assert_eq!(grown.cast::<u8>().read(), 0x5a);
            assert!(malloc_usable_size(grown) >= 128);
            free(grown);
        }
    }

    #[test]
    fn realloc_zero_frees() {
        // SAFETY: `realloc(p, 0)` frees `p`.
        unsafe {
            let ptr = malloc(16);
            assert!(!ptr.is_null());
            assert!(realloc(ptr, 0).is_null());
        }
    }

    #[test]
    fn calloc_zeroes_and_reports_overflow() {
        // SAFETY: matching calloc and free.
        unsafe {
            let ptr = calloc(16, 4);
            assert!(!ptr.is_null());
            assert_eq!(ptr.cast::<u8>().read(), 0);
            free(ptr);
            *libc::__errno_location() = 0;
            assert!(calloc(usize::MAX, 2).is_null());
            assert_eq!(*libc::__errno_location(), libc::ENOMEM);
        }
    }

    #[test]
    fn posix_memalign_reallocs_an_overaligned_extent() {
        let mut ptr = null_mut();
        // SAFETY: `memptr` is live and 8192 is a valid alignment.
        unsafe {
            assert_eq!(posix_memalign(&raw mut ptr, 8192, 32), 0);
            assert_eq!(ptr.addr() % 8192, 0);
            ptr.cast::<u8>().write(0xa5);
            let grown = realloc(ptr, 64);
            assert!(!grown.is_null());
            assert_eq!(grown.cast::<u8>().read(), 0xa5);
            free(grown);
        }
    }

    #[test]
    fn posix_memalign_returns_errors_without_changing_errno() {
        let mut ptr = null_mut();
        // SAFETY: `memptr` is a live output slot.
        unsafe { *libc::__errno_location() = libc::EBUSY };
        assert_eq!(unsafe { posix_memalign(&raw mut ptr, 3, 64) }, libc::EINVAL);
        assert!(ptr.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EBUSY);
    }

    #[test]
    fn aligned_alloc_checks_c11_constraints() {
        // SAFETY: invalid arguments return null.
        unsafe { *libc::__errno_location() = 0 };
        assert!(unsafe { aligned_alloc(64, 32) }.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EINVAL);
        assert!(unsafe { aligned_alloc(0, 0) }.is_null());
    }

    #[test]
    fn memalign_and_cfree_round_trip() {
        // SAFETY: matching memalign and cfree.
        unsafe {
            let ptr = memalign(32, 48);
            assert!(!ptr.is_null());
            assert_eq!(ptr.addr() % 32, 0);
            cfree(ptr);
        }
    }

    #[test]
    fn page_aligned_entries_round_trip() {
        // SAFETY: matching allocation and free calls.
        unsafe {
            let page = valloc(1);
            assert!(!page.is_null());
            assert_eq!(page.addr() % PAGE, 0);
            free(page);

            let rounded = pvalloc(PAGE + 1);
            assert!(!rounded.is_null());
            assert_eq!(rounded.addr() % PAGE, 0);
            assert!(malloc_usable_size(rounded) >= PAGE * 2);
            free(rounded);
        }
    }

    #[test]
    fn reallocarray_checks_multiplication() {
        // SAFETY: overflow returns null.
        unsafe { *libc::__errno_location() = 0 };
        assert!(unsafe { reallocarray(null_mut(), usize::MAX, 2) }.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::ENOMEM);
    }
}
