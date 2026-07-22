use core::alloc::Layout;
use core::num::NonZeroUsize;

/// A normalized, non-zero-size view of a `Layout`.
///
/// `align` is stored as `NonZeroUsize` so that alignment arithmetic below
/// never has to defend against a zero alignment; `Layout` already guarantees
/// alignment is a nonzero power of two.
#[derive(Clone, Copy)]
pub(crate) struct LayoutSpec {
    size: usize,
    align: NonZeroUsize,
}

impl LayoutSpec {
    pub(crate) const fn from_layout(layout: Layout) -> Self {
        let size = if layout.size() == 0 { 1 } else { layout.size() };
        // SAFETY: `Layout::align()` is always a nonzero power of two.
        let align = unsafe { NonZeroUsize::new_unchecked(layout.align()) };

        Self { size, align }
    }

    #[cfg(test)]
    pub(crate) fn from_size_align(size: usize, align: usize) -> Option<Self> {
        Layout::from_size_align(size, align)
            .ok()
            .map(Self::from_layout)
    }

    pub(crate) const fn size(self) -> usize {
        self.size
    }

    pub(crate) const fn align(self) -> usize {
        self.align.get()
    }

    pub(crate) fn minimum_block_size(self) -> usize {
        self.size.max(self.align.get())
    }

    pub(crate) fn align_addr(self, addr: usize) -> Option<usize> {
        let mask = self.align.get() - 1;
        addr.checked_add(mask).map(|value| value & !mask)
    }

    pub(crate) fn is_addr_aligned(self, addr: usize) -> bool {
        debug_assert!(self.align.is_power_of_two());

        addr & (self.align.get() - 1) == 0
    }

    /// Returns the page-rounded length of a mapping that can hold this
    /// allocation at any address while still allowing the returned pointer
    /// to be rounded up to `align`.
    ///
    /// The mapping must be at least `size + align` bytes: in the worst case
    /// the mapping's base address is already `1` byte past an aligned
    /// address, so up to `align` bytes of headroom before `size` bytes of
    /// usable space may be sacrificed to alignment.
    pub(crate) fn mapping_len(self, page_size: usize) -> Option<usize> {
        let size_with_align_headroom = self.size.checked_add(self.align.get())?;
        let mask = page_size.checked_sub(1)?;
        size_with_align_headroom
            .checked_add(mask)
            .map(|value| value & !mask)
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use super::*;

    #[test]
    fn layout_spec_normalizes_zero_size_to_one() {
        let layout = Layout::from_size_align(0, 8).unwrap();
        let spec = LayoutSpec::from_layout(layout);

        assert_eq!(spec.size(), 1);
    }

    #[test]
    fn layout_spec_preserves_alignment() {
        let spec = LayoutSpec::from_size_align(32, 64).unwrap();

        assert_eq!(spec.align(), 64);
    }

    #[test]
    fn layout_spec_minimum_block_size_is_max_size_or_align() {
        let spec = LayoutSpec::from_size_align(17, 64).unwrap();

        assert_eq!(spec.minimum_block_size(), 64);
    }

    #[test]
    fn layout_spec_align_addr_rounds_up() {
        let spec = LayoutSpec::from_size_align(1, 16).unwrap();

        assert_eq!(spec.align_addr(17), Some(32));
    }

    #[test]
    fn layout_spec_align_addr_keeps_aligned_addr() {
        let spec = LayoutSpec::from_size_align(1, 16).unwrap();

        assert_eq!(spec.align_addr(32), Some(32));
    }

    #[test]
    fn layout_spec_detects_aligned_address() {
        let spec = LayoutSpec::from_size_align(1, 16).unwrap();

        assert!(spec.is_addr_aligned(32));
        assert!(!spec.is_addr_aligned(33));
    }

    #[test]
    fn layout_spec_align_addr_detects_overflow() {
        let spec = LayoutSpec::from_size_align(1, 16).unwrap();

        assert_eq!(spec.align_addr(usize::MAX), None);
    }

    #[test]
    fn layout_spec_mapping_len_rounds_to_page() {
        let spec = LayoutSpec::from_size_align(4097, 8).unwrap();

        assert_eq!(spec.mapping_len(4096), Some(8192));
    }

    #[test]
    fn layout_spec_mapping_len_detects_overflow() {
        let spec = LayoutSpec {
            size: usize::MAX,
            align: NonZeroUsize::MIN,
        };

        assert_eq!(spec.mapping_len(4096), None);
    }
}
