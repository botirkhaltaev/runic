use crate::layout::LayoutSpec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SizeClassId {
    index: usize,
}

impl SizeClassId {
    pub(crate) const fn index(self) -> usize {
        self.index
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

        let align_power = usize::try_from(spec.align().trailing_zeros()).ok()?;
        let index = *ALIGNED_CLASS_BY_START
            .get(align_power)?
            .get(lower_bound_class(required))?;
        let block_size = *SIZES.get(index)?;

        Some(SizeClass {
            id: SizeClassId { index },
            block_size,
        })
    }
}

const SIZES: [usize; SizeClasses::COUNT] = [
    8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048, 3072,
    4096, 6144, 8192, 12288, 16384, 24576, 32768,
];

// Rows are indexed by `align.trailing_zeros()`; columns are lower-bound class indexes.
const ALIGNED_CLASS_BY_START: [[usize; SizeClasses::COUNT]; 16] = [
    IDENTITY_CLASS_MAP,
    IDENTITY_CLASS_MAP,
    IDENTITY_CLASS_MAP,
    IDENTITY_CLASS_MAP,
    [
        1, 1, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26,
    ],
    [
        3, 3, 3, 3, 5, 5, 7, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26,
    ],
    [
        5, 5, 5, 5, 5, 5, 8, 8, 8, 10, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26,
    ],
    [
        8, 8, 8, 8, 8, 8, 8, 8, 8, 11, 11, 11, 13, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26,
    ],
    [
        11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 14, 14, 14, 15, 16, 17, 18, 19, 20, 21, 22,
        23, 24, 25, 26,
    ],
    [
        14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 16, 16, 17, 18, 19, 20, 21, 22,
        23, 24, 25, 26,
    ],
    [
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 19, 20, 21, 22,
        23, 24, 25, 26,
    ],
    [
        18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 20, 20, 21, 22,
        23, 24, 25, 26,
    ],
    [
        20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 22, 22,
        23, 24, 25, 26,
    ],
    [
        22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22,
        24, 24, 25, 26,
    ],
    [
        24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
        24, 24, 26, 26,
    ],
    [
        26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26,
        26, 26, 26, 26,
    ],
];

const IDENTITY_CLASS_MAP: [usize; SizeClasses::COUNT] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26,
];

const fn lower_bound_class(size: usize) -> usize {
    match size {
        0..=8 => 0,
        9..=16 => 1,
        17..=24 => 2,
        25..=32 => 3,
        33..=48 => 4,
        49..=64 => 5,
        65..=80 => 6,
        81..=96 => 7,
        97..=128 => 8,
        129..=160 => 9,
        161..=192 => 10,
        193..=256 => 11,
        257..=320 => 12,
        321..=384 => 13,
        385..=512 => 14,
        513..=768 => 15,
        769..=1024 => 16,
        1025..=1536 => 17,
        1537..=2048 => 18,
        2049..=3072 => 19,
        3073..=4096 => 20,
        4097..=6144 => 21,
        6145..=8192 => 22,
        8193..=12288 => 23,
        12289..=16384 => 24,
        16385..=24576 => 25,
        _ => 26,
    }
}

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

    #[test]
    fn size_classes_match_linear_reference() {
        for size in 1..=SizeClasses::SMALL_MAX {
            for align in [
                1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
            ] {
                let spec = LayoutSpec::from_size_align(size, align).unwrap();
                let class = SizeClasses::get(spec).map(SizeClass::block_size);
                let reference = SIZES
                    .iter()
                    .copied()
                    .find(|block_size| *block_size >= size && block_size.is_multiple_of(align));

                assert_eq!(class, reference);
            }
        }
    }
}
