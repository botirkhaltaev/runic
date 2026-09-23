//! C malloc-family entry points.
//!
//! Compiled only for `c-abi` builds and tests. Each entry point is exported
//! unmangled under `c-abi` so `LD_PRELOAD` resolves libc's malloc family
//! here; tests call the same bodies as ordinary Rust functions, so a plain
//! `runic-alloc` dependency never replaces libc.
//!
//! `Layout` is built once per entry point, then the work is
//! `runic_core::Allocator`. Unlike `GlobalAlloc`, C `free` carries no size and
//! `free(NULL)` is a no-op.

use core::alloc::Layout;
use core::ffi::{c_int, c_void};
use core::ptr::null_mut;

use runic_core::Allocator;

/// `max_align_t` on Linux `x86_64`.
const MAX_ALIGN: usize = 16;
/// `valloc` alignment.
const PAGE: usize = 4096;

static ALLOC: Allocator = Allocator::new();

/// Return null after setting this thread's C `errno`.
#[cold]
fn errno_null(code: c_int) -> *mut c_void {
    // SAFETY: `__errno_location` returns this thread's live `errno` slot.
    unsafe { *libc::__errno_location() = code };
    null_mut()
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let Ok(layout) = Layout::from_size_align(size, MAX_ALIGN) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `layout` is well-formed; the caller owns the returned block.
    let ptr: *mut c_void = unsafe { ALLOC.alloc(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: a non-null `ptr` this allocator returned; anything else aborts.
    unsafe { ALLOC.free(ptr.cast()) };
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let Some(total) = nmemb.checked_mul(size) else {
        return errno_null(libc::ENOMEM);
    };
    let Ok(layout) = Layout::from_size_align(total, MAX_ALIGN) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `layout` is well-formed; the returned block is zeroed.
    let ptr: *mut c_void = unsafe { ALLOC.alloc_zeroed(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

/// C `realloc`. Owner comes from `PageMap`. POSIX does not require the result
/// to keep a `posix_memalign` alignment; the replacement uses `MAX_ALIGN`.
#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
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
    // SAFETY: `ptr` was returned by this allocator; `new` is well-formed.
    let resized: *mut c_void = unsafe { ALLOC.resize(ptr.cast(), new) }.cast();
    if resized.is_null() {
        return errno_null(libc::ENOMEM);
    }
    resized
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> i32 {
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
    // SAFETY: `memptr` is non-null and the caller owns the slot.
    unsafe { memptr.write(ptr.cast()) };
    0
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() || !size.is_multiple_of(alignment) {
        return errno_null(libc::EINVAL);
    }
    // SAFETY: C11 requires `size` to be a multiple of `alignment`, checked above.
    unsafe { memalign(alignment, size) }
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() {
        return errno_null(libc::EINVAL);
    }
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: `alignment` is a nonzero power of two, so `layout` is well-formed.
    let ptr: *mut c_void = unsafe { ALLOC.alloc(layout) }.cast();
    if ptr.is_null() {
        return errno_null(libc::ENOMEM);
    }
    ptr
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
    // SAFETY: the page size is a power of two.
    unsafe { memalign(PAGE, size) }
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn reallocarray(ptr: *mut c_void, nmemb: usize, size: usize) -> *mut c_void {
    let Some(total) = nmemb.checked_mul(size) else {
        return errno_null(libc::ENOMEM);
    };
    // SAFETY: same contract as `realloc`.
    unsafe { realloc(ptr, total) }
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn cfree(ptr: *mut c_void) {
    // SAFETY: same contract as `free`.
    unsafe { free(ptr) };
}

#[cfg_attr(feature = "c-abi", unsafe(no_mangle))]
unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> usize {
    ALLOC.usable_size(ptr.cast())
}

/// glibc's internal names. Some libraries call them instead of the public
/// ones, so a preloaded allocator has to own both; the linker needs distinct
/// symbols, not aliases.
#[cfg(feature = "c-abi")]
mod glibc {
    use core::ffi::c_void;

    macro_rules! forward {
        ($name:ident => $target:ident($($arg:ident: $ty:ty),*) -> $ret:ty) => {
            #[unsafe(no_mangle)]
            unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
                // SAFETY: same contract as the entry point it forwards to.
                unsafe { super::$target($($arg),*) }
            }
        };
        ($name:ident => $target:ident($($arg:ident: $ty:ty),*)) => {
            #[unsafe(no_mangle)]
            unsafe extern "C" fn $name($($arg: $ty),*) {
                // SAFETY: same contract as the entry point it forwards to.
                unsafe { super::$target($($arg),*) };
            }
        };
    }

    forward!(__libc_malloc => malloc(size: usize) -> *mut c_void);
    forward!(__libc_free => free(ptr: *mut c_void));
    forward!(__libc_calloc => calloc(nmemb: usize, size: usize) -> *mut c_void);
    forward!(__libc_realloc => realloc(ptr: *mut c_void, size: usize) -> *mut c_void);
    forward!(__libc_memalign => memalign(alignment: usize, size: usize) -> *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_null_is_noop() {
        // SAFETY: C `free(NULL)` is a no-op.
        unsafe { free(null_mut()) };
    }

    #[test]
    fn malloc_zero_returns_a_freeable_block() {
        // SAFETY: `malloc(0)` returns a unique freeable pointer.
        let ptr = unsafe { malloc(0) };
        assert!(!ptr.is_null());
        unsafe { free(ptr) };
    }

    #[test]
    fn malloc_realloc_free_preserves_prefix() {
        // SAFETY: matching malloc / realloc / free on one live block.
        unsafe {
            let ptr = malloc(64);
            assert!(!ptr.is_null());
            assert!(malloc_usable_size(ptr) >= 64);
            ptr.cast::<u8>().write(0x5a);
            let grown = realloc(ptr, 128);
            assert!(!grown.is_null());
            assert_eq!(grown.cast::<u8>().read(), 0x5a);
            assert!(malloc_usable_size(grown) >= 128);
            free(grown);
        }
    }

    #[test]
    fn realloc_null_allocates() {
        // SAFETY: `realloc(NULL, n)` is `malloc(n)`.
        unsafe {
            let ptr = realloc(null_mut(), 16);
            assert!(!ptr.is_null());
            free(ptr);
        }
    }

    #[test]
    fn realloc_zero_frees() {
        // SAFETY: `realloc(p, 0)` frees `p` and returns null.
        unsafe {
            let ptr = malloc(16);
            assert!(!ptr.is_null());
            assert!(realloc(ptr, 0).is_null());
        }
    }

    #[test]
    fn calloc_zeroes_and_rejects_overflow() {
        // SAFETY: valid count / size, then an overflowing pair.
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
    fn posix_memalign_returns_an_aligned_block() {
        let mut ptr = null_mut();
        // SAFETY: `memptr` is a live out-parameter; 64 is a valid alignment.
        unsafe {
            assert_eq!(posix_memalign(&raw mut ptr, 64, 32), 0);
            assert!(!ptr.is_null());
            assert_eq!(ptr.addr() % 64, 0);
            free(ptr);
        }
    }

    #[test]
    fn posix_memalign_extent_realloc_preserves_prefix() {
        let mut ptr = null_mut();
        // SAFETY: 8192 is a power of two above a page, so this is an extent.
        unsafe {
            assert_eq!(posix_memalign(&raw mut ptr, 8192, 32), 0);
            assert!(!ptr.is_null());
            assert_eq!(ptr.addr() % 8192, 0);
            ptr.cast::<u8>().write(0xa5);
            let grown = realloc(ptr, 64);
            assert!(!grown.is_null());
            assert_eq!(grown.cast::<u8>().read(), 0xa5);
            free(grown);
        }
    }

    #[test]
    fn posix_memalign_rejects_alignment_that_is_not_a_power_of_two() {
        let mut ptr = null_mut();
        // SAFETY: `memptr` is a live out-parameter; POSIX leaves errno unchanged.
        unsafe { *libc::__errno_location() = libc::EBUSY };
        assert_eq!(unsafe { posix_memalign(&raw mut ptr, 3, 64) }, libc::EINVAL);
        assert!(ptr.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EBUSY);
    }

    #[test]
    fn aligned_alloc_rejects_size_that_is_not_a_multiple() {
        // SAFETY: C11 rejects this pair and glibc reports `EINVAL`.
        unsafe { *libc::__errno_location() = 0 };
        assert!(unsafe { aligned_alloc(64, 32) }.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EINVAL);
        assert!(unsafe { aligned_alloc(0, 0) }.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EINVAL);
    }

    #[test]
    fn memalign_and_cfree_round_trip() {
        // SAFETY: matching memalign / cfree.
        unsafe {
            let ptr = memalign(32, 48);
            assert!(!ptr.is_null());
            assert_eq!(ptr.addr() % 32, 0);
            cfree(ptr);
        }
    }

    #[test]
    fn valloc_is_page_aligned_and_reallocarray_grows() {
        // SAFETY: matching valloc / reallocarray / free.
        unsafe {
            let ptr = valloc(1);
            assert!(!ptr.is_null());
            assert_eq!(ptr.addr() % PAGE, 0);
            let grown = reallocarray(ptr, 2, PAGE);
            assert!(!grown.is_null());
            free(grown);
        }
    }

    #[test]
    fn reallocarray_rejects_overflow() {
        // SAFETY: overflow must not allocate and is reported as `ENOMEM`.
        unsafe { *libc::__errno_location() = 0 };
        assert!(unsafe { reallocarray(null_mut(), usize::MAX, 2) }.is_null());
        assert_eq!(unsafe { *libc::__errno_location() }, libc::ENOMEM);
    }
}
