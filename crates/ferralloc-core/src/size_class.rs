use crate::layout::LayoutSpec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SizeClassId(usize);

impl SizeClassId {
    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SizeClass {
    id: SizeClassId,
    block_size: usize,
}

impl SizeClass {
    pub(crate) const fn id(self) -> SizeClassId {
        self.id
    }

    pub(crate) const fn block_size(self) -> usize {
        self.block_size
    }
}

pub(crate) struct SizeClasses;

impl SizeClasses {
    pub(crate) const COUNT: usize = 27;
    pub(crate) const SMALL_MAX: usize = 32 * 1024;

    pub(crate) fn get(spec: LayoutSpec) -> Option<SizeClass> {
        let required = spec.minimum_block_size();

        if required > Self::SMALL_MAX {
            return None;
        }

        for (index, &block_size) in SIZES.iter().enumerate() {
            if block_size >= spec.size() && block_size.is_multiple_of(spec.align()) {
                return Some(SizeClass {
                    id: SizeClassId(index),
                    block_size,
                });
            }
        }

        None
    }
}

const SIZES: [usize; SizeClasses::COUNT] = [
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use super::*;

    #[test]
    fn size_classes_map_one_byte_to_eight() {
        let spec = LayoutSpec::from_size_align(1, 1).unwrap();
        let class = SizeClasses::get(spec).unwrap();

        assert_eq!(class.block_size(), 8);
    }

    #[test]
    fn size_classes_map_exact_boundaries_to_themselves() {
        for &size in &SIZES {
            let spec = LayoutSpec::from_size_align(size, 1).unwrap();
            let class = SizeClasses::get(spec).unwrap();

            assert_eq!(class.block_size(), size);
        }
    }

    #[test]
    fn size_classes_reject_larger_than_small_max() {
        let spec = LayoutSpec::from_size_align(SizeClasses::SMALL_MAX + 1, 1).unwrap();

        assert!(SizeClasses::get(spec).is_none());
    }

    #[test]
    fn size_classes_choose_naturally_aligned_block() {
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::get(spec).unwrap();

        assert_eq!(class.block_size(), 32);
    }

    #[test]
    fn size_classes_return_only_classes_that_satisfy_layout() {
        for size in 1..=SizeClasses::SMALL_MAX {
            for align in [1, 2, 4, 8, 16, 32, 64, 128, 4096] {
                let layout = Layout::from_size_align(size, align).unwrap();
                let spec = LayoutSpec::from_layout(layout);
                let Some(class) = SizeClasses::get(spec) else {
                    continue;
                };

                assert!(class.block_size() >= size);
                assert!(class.block_size().is_multiple_of(align));
            }
        }
    }
}
