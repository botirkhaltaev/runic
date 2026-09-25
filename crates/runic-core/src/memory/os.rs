use core::{num::NonZeroUsize, ptr::NonNull};

use crate::{
    config::{Hints, HugePage, Numa},
    memory::AddressRange,
};

pub(crate) const PAGE_SIZE: usize = 4096;
/// `MPOL_PREFERRED` from `linux/mempolicy.h`; `libc` does not export it.
const MPOL_PREFERRED: libc::c_int = 1;
/// Linux supports node ids below `MAX_NUMNODES` (1024).
const NODE_BITS: usize = 1024;
const NODE_WORDS: usize = NODE_BITS / 64;

/// Sole owner of one live anonymous mmap region.
///
/// Constructed only by [`Memory`] map methods. `Drop` munmaps the region.
/// Length is always nonzero and a multiple of [`PAGE_SIZE`]; base is always
/// page-aligned.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Mapping {
    base: NonNull<u8>,
    len: NonZeroUsize,
}

// SAFETY: `Mapping` uniquely owns an mmap; moving ownership across threads is
// valid, shared access is immutable, and `Drop` requires exclusive ownership.
unsafe impl Send for Mapping {}
// SAFETY: shared methods expose only the immutable address range.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Private: every `Mapping` must describe a live mmap region owned uniquely
    /// by that `Mapping`, so construction is confined to this module.
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

    /// Apply payload hints. Each hint is independent and best effort.
    pub(crate) fn prefer(&self, hints: Hints) {
        if hints.hugepage() == HugePage::Thp {
            self.prefer_huge();
        }
        if hints.numa() == Numa::Local {
            self.prefer_local();
        }
    }

    /// Prefer transparent huge pages. Failure keeps ordinary pages.
    fn prefer_huge(&self) {
        // SAFETY: this mapping is live; advise failure leaves it mapped.
        unsafe {
            libc::madvise(
                self.base.as_ptr().cast(),
                self.len.get(),
                libc::MADV_HUGEPAGE,
            );
        }
    }

    /// Prefer the allocating thread's NUMA node.
    /// Failure keeps kernel first-touch placement.
    fn prefer_local(&self) {
        let mut node = 0u32;
        // SAFETY: SYS_getcpu writes this thread's node; a null CPU out-pointer
        // is allowed.
        let queried = unsafe {
            libc::syscall(
                libc::SYS_getcpu,
                core::ptr::null_mut::<u32>(),
                &raw mut node,
                core::ptr::null_mut::<u8>(),
            )
        };
        if queried != 0 {
            return;
        }
        let Ok(node) = usize::try_from(node) else {
            return;
        };
        if node >= NODE_BITS {
            return;
        }

        let mut mask = [0u64; NODE_WORDS];
        let Some(word) = mask.get_mut(node / 64) else {
            return;
        };
        *word = 1u64 << (node % 64);
        // SAFETY: this mapping is live and `mask` covers the queried node bit.
        unsafe {
            libc::syscall(
                libc::SYS_mbind,
                self.base.as_ptr(),
                self.len.get(),
                MPOL_PREFERRED,
                mask.as_ptr(),
                NODE_BITS,
                0,
            );
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: Mapping owns the region returned by this module's map calls.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len.get()) };
    }
}

/// Virtual-memory surface every mapping goes through.
///
/// Callers use the [`Os`](super::Os) alias; [`Linux`] is today's only impl and
/// holds every `libc` call. Payload hints live on [`Mapping`].
pub(crate) trait Memory {
    fn page_size() -> usize;

    /// Anonymous private map of `len`, page-rounded.
    fn map(len: usize) -> Option<Mapping>;

    /// Anonymous private map of `len` whose base is aligned to `align`
    /// (power of two, at least one page).
    fn map_aligned(len: usize, align: usize) -> Option<Mapping>;

    /// Drop resident pages in `range`, keeping the mapping.
    ///
    /// Returns whether the OS accepted it. Run discard ignores the result;
    /// extent `Discard` uses it to decide whether zeroed reuse can skip memset.
    fn discard(range: AddressRange) -> bool;

    fn round_to_page(len: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(len.checked_next_multiple_of(Self::page_size())?)
    }

    /// Extent payload map, then hints.
    fn map_payload(len: usize, hints: Hints) -> Option<Mapping> {
        let mapping = Self::map(len)?;
        mapping.prefer(hints);
        Some(mapping)
    }

    /// Run payload map at `align`, then hints.
    fn map_aligned_payload(len: usize, align: usize, hints: Hints) -> Option<Mapping> {
        let mapping = Self::map_aligned(len, align)?;
        mapping.prefer(hints);
        Some(mapping)
    }
}

/// Linux `mmap` / `madvise` / `mbind`.
pub(crate) struct Linux;

impl Memory for Linux {
    fn page_size() -> usize {
        PAGE_SIZE
    }

    fn map(len: usize) -> Option<Mapping> {
        if len == 0 {
            return None;
        }
        let rounded_len = Self::round_to_page(len)?;
        // SAFETY: mmap is called with a null hint and a page-rounded length.
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

    /// Over-maps by `align`, then trims the head and tail so the kept region is
    /// `len` (page-rounded) bytes at an aligned address.
    fn map_aligned(len: usize, align: usize) -> Option<Mapping> {
        if len == 0 || align < PAGE_SIZE || !align.is_power_of_two() {
            return None;
        }

        let keep = Self::round_to_page(len)?;
        let total = keep.get().checked_add(align)?;
        // SAFETY: anonymous private mapping, page-rounded over-map length.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }

        let raw = ptr.addr();
        let Some(aligned) = raw.checked_next_multiple_of(align) else {
            // SAFETY: this thread uniquely owns the failed over-map.
            unsafe { libc::munmap(ptr, total) };
            return None;
        };
        let head = aligned - raw;
        let Some(kept_end) = head.checked_add(keep.get()) else {
            // SAFETY: this thread uniquely owns the failed over-map.
            unsafe { libc::munmap(ptr, total) };
            return None;
        };
        let Some(tail) = total.checked_sub(kept_end) else {
            // SAFETY: this thread uniquely owns the failed over-map.
            unsafe { libc::munmap(ptr, total) };
            return None;
        };
        let Some(base) = NonNull::new(ptr.cast::<u8>().wrapping_byte_add(head)) else {
            // SAFETY: this thread uniquely owns the failed over-map.
            unsafe { libc::munmap(ptr, total) };
            return None;
        };
        if !base.as_ptr().addr().is_multiple_of(align) {
            // SAFETY: this thread uniquely owns the failed over-map.
            unsafe { libc::munmap(ptr, total) };
            return None;
        }

        // SAFETY: `head`/`tail` are in-range prefixes/suffixes of this mmap.
        unsafe {
            if head > 0 {
                libc::munmap(ptr, head);
            }
            if tail > 0 {
                libc::munmap(base.as_ptr().add(keep.get()).cast(), tail);
            }
        }

        Some(Mapping::new(base, keep))
    }

    fn discard(range: AddressRange) -> bool {
        let len = range.len();
        if len == 0 {
            return true;
        }
        // SAFETY: caller keeps the mapping; this only advises the kernel.
        unsafe { libc::madvise(range.base().as_ptr().cast(), len, libc::MADV_DONTNEED) == 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Os;
    use super::*;

    #[test]
    fn os_round_to_page_keeps_page_sized_value() {
        assert_eq!(
            Os::round_to_page(PAGE_SIZE).map(NonZeroUsize::get),
            Some(PAGE_SIZE)
        );
    }

    #[test]
    fn os_round_to_page_rounds_up() {
        assert_eq!(
            Os::round_to_page(PAGE_SIZE + 1).map(NonZeroUsize::get),
            Some(PAGE_SIZE * 2)
        );
    }

    #[test]
    fn os_round_to_page_detects_overflow() {
        assert_eq!(Os::round_to_page(usize::MAX), None);
    }

    #[test]
    fn os_round_to_page_rejects_zero() {
        assert_eq!(Os::round_to_page(0), None);
    }

    #[test]
    fn os_map_rejects_zero() {
        assert!(Os::map(0).is_none());
    }

    #[test]
    fn os_map_returns_page_aligned_mapping() {
        let mapping = Os::map(1).unwrap();

        assert_eq!(mapping.base().as_ptr() as usize % PAGE_SIZE, 0);
        assert_eq!(mapping.len().get(), PAGE_SIZE);

        drop(mapping);
    }

    #[test]
    fn os_map_aligned_rejects_zero_and_small_align() {
        assert!(Os::map_aligned(0, PAGE_SIZE).is_none());
        assert!(Os::map_aligned(PAGE_SIZE, PAGE_SIZE / 2).is_none());
        assert!(Os::map_aligned(PAGE_SIZE, PAGE_SIZE + 1).is_none());
    }

    #[test]
    fn os_map_aligned_returns_aligned_mapping() {
        let align = 64 * 1024;
        let mapping = Os::map_aligned(align + PAGE_SIZE, align).unwrap();

        assert_eq!(mapping.base().as_ptr() as usize % align, 0);
        assert_eq!(mapping.len().get(), align + PAGE_SIZE);

        drop(mapping);
    }

    #[test]
    fn os_aligned_payload_rejects_invalid_alignment() {
        let hints = Hints::new();
        assert!(Os::map_aligned_payload(PAGE_SIZE, 0, hints).is_none());
        assert!(Os::map_aligned_payload(PAGE_SIZE, PAGE_SIZE + 1, hints).is_none());
    }

    #[test]
    fn os_mapping_is_writable() {
        let mapping = Os::map(PAGE_SIZE).unwrap();

        unsafe {
            mapping.base().as_ptr().write(0xab);
            mapping.base().as_ptr().add(PAGE_SIZE - 1).write(0xcd);
            assert_eq!(mapping.base().as_ptr().read(), 0xab);
            assert_eq!(mapping.base().as_ptr().add(PAGE_SIZE - 1).read(), 0xcd);
        }
    }

    #[test]
    fn os_discard_dontneed_zeros_anonymous_page() {
        let mapping = Os::map(PAGE_SIZE).unwrap();
        unsafe {
            mapping.base().as_ptr().write(0xab);
            assert_eq!(mapping.base().as_ptr().read(), 0xab);
        }
        assert!(Os::discard(mapping.range()));
        unsafe {
            assert_eq!(mapping.base().as_ptr().read(), 0);
        }
    }

    #[test]
    fn os_discard_empty_range_is_noop() {
        let mapping = Os::map(PAGE_SIZE).unwrap();
        assert!(Os::discard(AddressRange::new(mapping.base(), 0)));
        unsafe {
            mapping.base().as_ptr().write(0xcd);
            assert_eq!(mapping.base().as_ptr().read(), 0xcd);
        }
    }

    #[test]
    fn os_thp_payload_stays_writable() {
        let mapping =
            Os::map_payload(PAGE_SIZE, Hints::new().with_hugepage(HugePage::Thp)).unwrap();
        unsafe {
            mapping.base().as_ptr().write(0x11);
            assert_eq!(mapping.base().as_ptr().read(), 0x11);
        }
    }

    #[test]
    fn os_local_payload_stays_writable() {
        let mapping = Os::map_payload(PAGE_SIZE, Hints::new().with_numa(Numa::Local)).unwrap();
        unsafe {
            mapping.base().as_ptr().write(0x22);
            assert_eq!(mapping.base().as_ptr().read(), 0x22);
        }
    }
}
