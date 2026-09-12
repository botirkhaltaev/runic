use core::{
    mem::size_of,
    num::NonZeroUsize,
    ptr::{self, NonNull},
};

const PAGE: usize = 4096;

pub(crate) fn region_len(cpus: u32) -> Option<NonZeroUsize> {
    let bytes = usize::try_from(cpus)
        .ok()?
        .checked_mul(size_of::<usize>())?;
    let pages = bytes.div_ceil(PAGE).checked_mul(PAGE)?;
    NonZeroUsize::new(pages)
}

/// Word at `base + cpu * size_of::<usize>()`. Address-based so alignment is not a cast.
///
/// # Safety
/// `cpu` is in range and `base` is a live region of `cpus` words.
#[inline]
pub(crate) unsafe fn word(base: NonNull<u8>, cpu: u32) -> NonNull<usize> {
    let addr = base
        .as_ptr()
        .addr()
        .wrapping_add((cpu as usize).wrapping_mul(size_of::<usize>()));
    let ptr: *mut usize = ptr::with_exposed_provenance_mut(addr);
    // SAFETY: mmap of usizes is aligned; `cpu` is in range.
    unsafe { NonNull::new_unchecked(ptr) }
}

pub(crate) struct Region {
    base: NonNull<u8>,
    len: NonZeroUsize,
}

impl Region {
    pub(crate) fn map(len: NonZeroUsize) -> Option<Self> {
        // SAFETY: anonymous private mapping, page-rounded length.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len.get(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(Self {
            base: NonNull::new(ptr.cast())?,
            len,
        })
    }

    pub(crate) const fn into_raw(self) -> (NonNull<u8>, NonZeroUsize) {
        let base = self.base;
        let len = self.len;
        core::mem::forget(self);
        (base, len)
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: `Region` uniquely owns this mmap.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.len.get());
        }
    }
}
