use core::{mem::size_of, num::NonZeroUsize, ptr::NonNull, sync::atomic::AtomicUsize};

const PAGE: usize = 4096;

pub(crate) fn region_len(cpus: u32) -> Option<NonZeroUsize> {
    let bytes = usize::try_from(cpus)
        .ok()?
        .checked_mul(size_of::<AtomicUsize>())?;
    let pages = bytes.div_ceil(PAGE).checked_mul(PAGE)?;
    NonZeroUsize::new(pages)
}

/// Word at `base + cpu * size_of::<AtomicUsize>()`.
///
/// # Safety
/// `cpu` is in range and `base` is a live region of `cpus` aligned words.
#[inline]
pub(crate) unsafe fn word(base: NonNull<u8>, cpu: u32) -> NonNull<AtomicUsize> {
    let offset = (cpu as usize).wrapping_mul(size_of::<AtomicUsize>());
    // SAFETY: `cpu` is in range; mmap of usizes is aligned.
    unsafe { NonNull::new_unchecked(base.as_ptr().add(offset).cast()) }
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

    pub(crate) const fn base(&self) -> NonNull<u8> {
        self.base
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
