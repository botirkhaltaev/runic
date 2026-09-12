use core::{
    mem::size_of,
    num::NonZeroUsize,
    ptr::{self, NonNull},
};

pub(crate) const HEADER_SIZE: usize = 8;
pub(crate) const SLOT_SIZE: usize = 8;
const PAGE: usize = 4096;

#[repr(C, align(8))]
pub(crate) struct Header {
    pub current: u32,
    pub capacity: u32,
}

pub(crate) fn block_shift(cap: u32) -> Option<u8> {
    let slots = usize::try_from(cap).ok()?;
    if slots == 0 {
        return None;
    }
    let bytes = HEADER_SIZE.checked_add(slots.checked_mul(SLOT_SIZE)?)?;
    let pow2 = bytes.checked_next_power_of_two()?;
    let shift = pow2.trailing_zeros();
    u8::try_from(shift).ok().filter(|&s| s < 32)
}

pub(crate) fn region_len(cpus: u32, shift: u8, extra: usize) -> Option<NonZeroUsize> {
    let cpus = usize::try_from(cpus).ok()?;
    let block = 1usize.checked_shl(u32::from(shift))?;
    let slabs = cpus.checked_mul(block)?;
    let raw = slabs.checked_add(extra)?;
    let pages = raw.div_ceil(PAGE).checked_mul(PAGE)?;
    NonZeroUsize::new(pages)
}

/// Header at `base + (cpu << shift)`. Address-based so alignment is not a cast.
///
/// # Safety
/// `cpu` is in range and `base` is a live region with this `shift`.
#[inline]
pub(crate) unsafe fn header(base: NonNull<u8>, cpu: u32, shift: u8) -> NonNull<Header> {
    let addr = base.as_ptr().addr().wrapping_add((cpu as usize) << shift);
    let ptr: *mut Header = ptr::with_exposed_provenance_mut(addr);
    // SAFETY: mmap + power-of-two stride is `Header`-aligned; `cpu` is in range.
    unsafe { NonNull::new_unchecked(ptr) }
}

/// Slot `index` immediately after `hdr`. Address-based so alignment is not a cast.
///
/// # Safety
/// `index` is in range for the slab at `hdr`.
#[inline]
pub(crate) unsafe fn slot<T>(hdr: NonNull<Header>, index: u32) -> NonNull<NonNull<T>> {
    let addr = hdr
        .as_ptr()
        .addr()
        .wrapping_add(size_of::<Header>())
        .wrapping_add((index as usize).wrapping_mul(size_of::<NonNull<T>>()));
    let ptr: *mut NonNull<T> = ptr::with_exposed_provenance_mut(addr);
    // SAFETY: slots follow an 8-byte header; `index` is in range.
    unsafe { NonNull::new_unchecked(ptr) }
}

/// # Safety
/// `base` is a zeroed mapping with `cpus` blocks of `1 << shift` bytes.
pub(crate) unsafe fn init_headers(base: NonNull<u8>, cpus: u32, shift: u8, cap: u32) {
    for cpu in 0..cpus {
        // SAFETY: `cpu` is in `0..cpus`.
        let hdr = unsafe { header(base, cpu, shift) };
        // SAFETY: each block is a zeroed mmap we own; header is at offset 0.
        unsafe {
            hdr.as_ptr().write(Header {
                current: 0,
                capacity: cap,
            });
        }
    }
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

// SAFETY: the mapping is process-private anonymous memory.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}
