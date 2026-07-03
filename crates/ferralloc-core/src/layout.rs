use core::alloc::Layout;

#[derive(Clone, Copy)]
pub(crate) struct LayoutSpec {
    size: usize,
    align: usize,
}

impl LayoutSpec {
    pub(crate) const fn from_layout(layout: Layout) -> Self {
        let size = if layout.size() == 0 { 1 } else { layout.size() };

        Self {
            size,
            align: layout.align(),
        }
    }

    pub(crate) fn from_size_align(size: usize, align: usize) -> Option<Self> {
        Layout::from_size_align(size, align)
            .ok()
            .map(Self::from_layout)
    }

    pub(crate) const fn size(self) -> usize {
        self.size
    }

    pub(crate) const fn align(self) -> usize {
        self.align
    }

    pub(crate) fn minimum_block_size(self) -> usize {
        self.size.max(self.align)
    }

    pub(crate) fn align_addr(self, addr: usize) -> Option<usize> {
        let mask = self.align.checked_sub(1)?;
        addr.checked_add(mask).map(|value| value & !mask)
    }

    pub(crate) fn mapping_len(self, page_size: usize) -> Option<usize> {
        let len = self.size.checked_add(self.align)?;
        let mask = page_size.checked_sub(1)?;
        len.checked_add(mask).map(|value| value & !mask)
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
            align: 1,
        };

        assert_eq!(spec.mapping_len(4096), None);
    }
}
