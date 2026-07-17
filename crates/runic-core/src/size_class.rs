use crate::{layout::LayoutSpec, memory::PAGE_SIZE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SizeClassId {
    index: usize,
}

impl SizeClassId {
    pub(crate) const fn index(self) -> usize {
        self.index
    }

    const fn new(index: usize) -> Self {
        Self { index }
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
    const MIN_ALIGNMENT: usize = 8;
    const ALIGN_POWER_COUNT: usize = 13;
    const SIZES: [usize; Self::COUNT] = [
        8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048,
        3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768,
    ];
    const ALIGNED_CLASS_BY_START: [[usize; Self::COUNT]; Self::ALIGN_POWER_COUNT] = [
        Self::IDENTITY_CLASS_MAP,
        Self::IDENTITY_CLASS_MAP,
        Self::IDENTITY_CLASS_MAP,
        Self::IDENTITY_CLASS_MAP,
        [
            1, 1, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26,
        ],
        [
            3, 3, 3, 3, 5, 5, 7, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26,
        ],
        [
            5, 5, 5, 5, 5, 5, 8, 8, 8, 10, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26,
        ],
        [
            8, 8, 8, 8, 8, 8, 8, 8, 8, 11, 11, 11, 13, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26,
        ],
        [
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 14, 14, 14, 15, 16, 17, 18, 19, 20, 21,
            22, 23, 24, 25, 26,
        ],
        [
            14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 16, 16, 17, 18, 19, 20, 21,
            22, 23, 24, 25, 26,
        ],
        [
            16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 19, 20, 21,
            22, 23, 24, 25, 26,
        ],
        [
            18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 20, 20, 21,
            22, 23, 24, 25, 26,
        ],
        [
            20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 22,
            22, 23, 24, 25, 26,
        ],
    ];
    const IDENTITY_CLASS_MAP: [usize; Self::COUNT] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26,
    ];

    #[cfg(test)]
    pub(crate) fn for_layout(spec: LayoutSpec) -> Option<SizeClass> {
        Self::class(Self::id_for(spec)?)
    }

    pub(crate) fn id_for(spec: LayoutSpec) -> Option<SizeClassId> {
        let required = spec.minimum_block_size();

        if required > Self::SMALL_MAX {
            return None;
        }

        if spec.align() > PAGE_SIZE {
            return None;
        }

        let lower_bound = Self::lower_bound_index(required)?;
        if spec.align() <= Self::MIN_ALIGNMENT {
            return Some(SizeClassId::new(lower_bound));
        }

        Self::aligned_class_from(lower_bound, spec.align())
    }

    pub(crate) fn class(id: SizeClassId) -> Option<SizeClass> {
        let index = id.index();
        let block_size = *Self::SIZES.get(index)?;

        Some(SizeClass { id, block_size })
    }

    fn lower_bound_index(required: usize) -> Option<usize> {
        let index = match required {
            1..=32 => Self::class_in_tier(required, 1, 0, 8),
            33..=96 => Self::class_in_tier(required, 33, 4, 16),
            97..=128 => 8,
            129..=192 => Self::class_in_tier(required, 129, 9, 32),
            193..=384 => Self::class_in_tier(required, 193, 11, 64),
            385..=512 => 14,
            513..=1024 => Self::class_in_tier(required, 513, 15, 256),
            1025..=2048 => Self::class_in_tier(required, 1025, 17, 512),
            2049..=4096 => Self::class_in_tier(required, 2049, 19, 1024),
            4097..=8192 => Self::class_in_tier(required, 4097, 21, 2048),
            8193..=16384 => Self::class_in_tier(required, 8193, 23, 4096),
            16385..=32768 => Self::class_in_tier(required, 16385, 25, 8192),
            _ => return None,
        };

        Some(index)
    }

    const fn class_in_tier(
        size: usize,
        first_size: usize,
        first_class: usize,
        quantum: usize,
    ) -> usize {
        first_class + ((size - first_size) / quantum)
    }

    fn aligned_class_from(start: usize, align: usize) -> Option<SizeClassId> {
        debug_assert!(align.is_power_of_two());
        let align_power = usize::try_from(align.trailing_zeros()).ok()?;
        Self::ALIGNED_CLASS_BY_START
            .get(align_power)?
            .get(start)
            .copied()
            .map(SizeClassId::new)
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use super::*;

    #[test]
    fn size_classes_map_one_byte_to_eight() {
        let spec = LayoutSpec::from_size_align(1, 1).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();

        assert_eq!(class.block_size(), 8);
    }

    #[test]
    fn size_classes_map_exact_boundaries_to_themselves() {
        for &size in &SizeClasses::SIZES {
            let spec = LayoutSpec::from_size_align(size, 1).unwrap();
            let class = SizeClasses::for_layout(spec).unwrap();

            assert_eq!(class.block_size(), size);
        }
    }

    #[test]
    fn size_classes_reject_larger_than_small_max() {
        let spec = LayoutSpec::from_size_align(SizeClasses::SMALL_MAX + 1, 1).unwrap();

        assert!(SizeClasses::for_layout(spec).is_none());
    }

    #[test]
    fn size_classes_reject_over_page_alignment() {
        let spec = LayoutSpec::from_size_align(1, PAGE_SIZE * 2).unwrap();

        assert!(SizeClasses::for_layout(spec).is_none());
    }

    #[test]
    fn size_classes_choose_naturally_aligned_block() {
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();

        assert_eq!(class.block_size(), 32);
    }

    #[test]
    fn size_classes_return_only_classes_that_satisfy_layout() {
        for size in 1..=SizeClasses::SMALL_MAX {
            for align in [1, 2, 4, 8, 16, 32, 64, 128, 4096] {
                let layout = Layout::from_size_align(size, align).unwrap();
                let spec = LayoutSpec::from_layout(layout);
                let Some(class) = SizeClasses::for_layout(spec) else {
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
                let class = SizeClasses::for_layout(spec).map(SizeClass::block_size);
                let reference = if align > PAGE_SIZE {
                    None
                } else {
                    SizeClasses::SIZES
                        .iter()
                        .copied()
                        .find(|block_size| *block_size >= size && block_size.is_multiple_of(align))
                };

                assert_eq!(class, reference);
            }
        }
    }

    #[test]
    fn size_classes_are_sorted() {
        for sizes in SizeClasses::SIZES.windows(2) {
            let [left, right] = sizes else {
                unreachable!();
            };

            assert!(left < right);
        }
    }

    #[test]
    fn size_classes_are_minimum_aligned() {
        for block_size in SizeClasses::SIZES {
            assert!(block_size.is_multiple_of(SizeClasses::MIN_ALIGNMENT));
        }
    }

    #[test]
    fn size_classes_small_max_is_largest_class() {
        assert_eq!(SizeClasses::SIZES.last(), Some(&SizeClasses::SMALL_MAX));
    }

    #[test]
    fn size_classes_alignment_table_covers_page_alignment() {
        assert_eq!(1_usize << (SizeClasses::ALIGN_POWER_COUNT - 1), PAGE_SIZE);
    }

    #[test]
    fn size_class_lower_bounds_match_declared_sizes() {
        for size in 1..=SizeClasses::SMALL_MAX {
            let index = SizeClasses::lower_bound_index(size).unwrap();
            let block_size = SizeClasses::SIZES.get(index).copied();
            let reference = SizeClasses::SIZES
                .iter()
                .copied()
                .find(|block_size| *block_size >= size);

            assert_eq!(block_size, reference);
        }
    }
}
