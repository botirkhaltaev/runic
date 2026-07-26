use core::{num::NonZeroUsize, ptr::NonNull};

use crate::memory::AddressRange;

pub(crate) const PAGE_SIZE: usize = 4096;

/// Sole owner of one live anonymous mmap region.
///
/// Constructed only by [`OsMemory::map`] / [`OsMemory::map_aligned`]. `Drop`
/// munmaps the region. Length is always nonzero and a multiple of [`PAGE_SIZE`];
/// base is always page-aligned (and segment-aligned after `map_aligned`).
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Mapping {
    base: NonNull<u8>,
    len: NonZeroUsize,
}

impl Mapping {
    /// Private: every `Mapping` must describe a live mmap region owned uniquely
    /// by that `Mapping`, so construction is confined to `OsMemory::map` /
    /// `OsMemory::map_aligned`.
    fn new(base: NonNull<u8>, len: NonZeroUsize) -> Self {
        debug_assert!(base.as_ptr().addr().is_multiple_of(PAGE_SIZE));
        debug_assert!(len.get().is_multiple_of(PAGE_SIZE));
        Self { base, len }
    }

    pub(crate) const fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) const fn len(&self) -> NonZeroUsize {
        self.len
    }

    pub(crate) const fn range(&self) -> AddressRange {
        AddressRange::new(self.base, self.len.get())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: Mapping owns an mmap allocation returned by OsMemory::map.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len.get()) };
    }
}

// SAFETY: Mapping owns a process-private mmap region. Moving ownership to another
// thread does not permit concurrent mutation of allocator metadata.
unsafe impl Send for Mapping {}

pub(crate) struct OsMemory;

impl OsMemory {
    pub(crate) const fn page_size() -> usize {
        PAGE_SIZE
    }

    pub(crate) fn map(len: usize) -> Option<Mapping> {
        if len == 0 {
            return None;
        }

        let rounded_len = Self::round_to_page(len)?;
        let rounded_len = NonZeroUsize::new(rounded_len)?;
        // SAFETY: mmap is called with a null hint, anonymous private mapping, and a page-rounded length.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                rounded_len.get(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return None;
        }

        NonNull::new(ptr.cast::<u8>()).map(|base| Mapping::new(base, rounded_len))
    }

    /// Map `len` bytes with `align`-aligned base (`align` and `len` must be page multiples).
    ///
    /// Used for run segments so any block pointer recovers the header via
    /// `ptr & !(align - 1)`.
    pub(crate) fn map_aligned(len: usize, align: usize) -> Option<Mapping> {
        if len == 0 || align < PAGE_SIZE || !align.is_power_of_two() {
            return None;
        }
        if !len.is_multiple_of(PAGE_SIZE) || !align.is_multiple_of(PAGE_SIZE) {
            return None;
        }

        let total = len.checked_add(align)?;
        let oversized = Self::map(total)?;
        let raw = oversized.base().as_ptr().addr();
        let aligned = (raw.checked_add(align - 1)?) & !(align - 1);
        let prefix = aligned - raw;
        let suffix = oversized.len().get() - prefix - len;

        // Disarm Drop; we munmap the edges and re-wrap the aligned window.
        let oversized = core::mem::ManuallyDrop::new(oversized);
        let base = oversized.base();

        if prefix > 0 {
            // SAFETY: prefix is a leading page-multiple of the anonymous mapping we own.
            unsafe {
                libc::munmap(base.as_ptr().cast(), prefix);
            }
        }
        if suffix > 0 {
            // SAFETY: suffix is a trailing page-multiple of the same anonymous mapping.
            unsafe {
                libc::munmap(base.as_ptr().wrapping_add(prefix + len).cast(), suffix);
            }
        }

        let aligned_base = NonNull::new(base.as_ptr().wrapping_add(prefix))?;
        let aligned_len = NonZeroUsize::new(len)?;
        Some(Mapping::new(aligned_base, aligned_len))
    }

    pub(crate) fn round_to_page(len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }

        let mask = PAGE_SIZE - 1;
        len.checked_add(mask).map(|value| value & !mask)
    }

    /// True when the page containing `ptr` is readable in this process.
    ///
    /// Used before segment-header loads so mask→base never SEGV on holes
    /// (e.g. extent pointers that mask into unmapped VA).
    pub(crate) fn page_readable(ptr: NonNull<u8>) -> bool {
        let page_addr = ptr.as_ptr().addr() & !(PAGE_SIZE - 1);
        let page = core::ptr::with_exposed_provenance::<libc::c_void>(page_addr);
        let mut fds = [-1, -1];
        // SAFETY: pipe with a two-slot fd array; write probes readability via EFAULT.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return false;
        }
        // SAFETY: write copies from `page`; returns EFAULT when unmapped (no signal).
        let wrote = unsafe { libc::write(fds[1], page, 1) };
        // SAFETY: both ends were opened by this pipe call.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        wrote == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_memory_round_to_page_keeps_page_sized_value() {
        assert_eq!(OsMemory::round_to_page(PAGE_SIZE), Some(PAGE_SIZE));
    }

    #[test]
    fn os_memory_round_to_page_rounds_up() {
        assert_eq!(OsMemory::round_to_page(PAGE_SIZE + 1), Some(PAGE_SIZE * 2));
    }

    #[test]
    fn os_memory_round_to_page_detects_overflow() {
        assert_eq!(OsMemory::round_to_page(usize::MAX), None);
    }

    #[test]
    fn os_memory_round_to_page_rejects_zero() {
        assert_eq!(OsMemory::round_to_page(0), None);
    }

    #[test]
    fn os_memory_map_rejects_zero() {
        assert!(OsMemory::map(0).is_none());
    }

    #[test]
    fn os_memory_map_returns_page_aligned_mapping() {
        let mapping = OsMemory::map(1).unwrap();

        assert_eq!(mapping.base().as_ptr() as usize % PAGE_SIZE, 0);
        assert_eq!(mapping.len().get(), PAGE_SIZE);

        drop(mapping);
    }

    #[test]
    fn os_memory_map_aligned_returns_align_aligned_base() {
        const ALIGN: usize = 128 * 1024;
        let mapping = OsMemory::map_aligned(ALIGN, ALIGN).unwrap();

        assert_eq!(mapping.base().as_ptr().addr() % ALIGN, 0);
        assert_eq!(mapping.len().get(), ALIGN);
        assert_eq!(mapping.base().as_ptr().addr() % PAGE_SIZE, 0);
    }

    #[test]
    fn os_memory_map_aligned_rejects_bad_align() {
        assert!(OsMemory::map_aligned(PAGE_SIZE, 0).is_none());
        assert!(OsMemory::map_aligned(PAGE_SIZE, PAGE_SIZE / 2).is_none());
        assert!(OsMemory::map_aligned(PAGE_SIZE, PAGE_SIZE + 1).is_none());
        assert!(OsMemory::map_aligned(0, PAGE_SIZE).is_none());
    }

    #[test]
    fn os_memory_mapping_is_writable() {
        let mapping = OsMemory::map(PAGE_SIZE).unwrap();

        unsafe {
            mapping.base().as_ptr().write(0xab);
            mapping.base().as_ptr().add(PAGE_SIZE - 1).write(0xcd);
            assert_eq!(mapping.base().as_ptr().read(), 0xab);
            assert_eq!(mapping.base().as_ptr().add(PAGE_SIZE - 1).read(), 0xcd);
        }
    }
}
