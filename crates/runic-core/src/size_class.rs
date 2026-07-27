use crate::{layout::LayoutSpec, memory::PAGE_SIZE};

/// Index into [`SizeClasses::SIZES`].
///
/// Only [`SizeClasses`] can construct this type, and only for in-range indexes,
/// so hot paths may treat `index()` as a trusted subscript into size-class
/// arrays of length [`SizeClasses::COUNT`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SizeClassId {
    index: usize,
}

impl SizeClassId {
    pub(crate) const fn index(self) -> usize {
        self.index
    }

    /// Returns an id for `index`, or `None` when out of range for `SIZES`.
    const fn new(index: usize) -> Option<Self> {
        if index < SizeClasses::COUNT {
            Some(Self { index })
        } else {
            None
        }
    }

    /// `index` must be in `0..SizeClasses::COUNT` (e.g. from `CLASS_FOR_SIZE`).
    const unsafe fn new_unchecked(index: usize) -> Self {
        debug_assert!(index < SizeClasses::COUNT);
        Self { index }
    }

    /// Byte offset of block `index` within a run payload for this class.
    #[inline]
    pub(crate) fn block_offset(self, index: usize) -> usize {
        match self.index() {
            0 => index << 3,
            1 => index << 4,
            2 => index * 24,
            3 => index << 5,
            4 => index * 48,
            5 => index << 6,
            6 => index * 80,
            7 => index * 96,
            8 => index << 7,
            9 => index * 160,
            10 => index * 192,
            11 => index << 8,
            12 => index * 320,
            13 => index * 384,
            14 => index << 9,
            15 => index * 768,
            16 => index << 10,
            17 => index * 1536,
            18 => index << 11,
            19 => index * 3072,
            20 => index << 12,
            21 => index * 6144,
            22 => index << 13,
            23 => index * 12288,
            24 => index << 14,
            25 => index * 24576,
            26 => index << 15,
            _ => unreachable!(),
        }
    }

    /// Block index from a payload offset; rejects interior and misaligned offsets.
    #[inline]
    pub(crate) fn block_index_from_offset(self, offset: usize) -> Option<usize> {
        match self.index() {
            0 => (offset.trailing_zeros() >= 3).then_some(offset >> 3),
            1 => (offset.trailing_zeros() >= 4).then_some(offset >> 4),
            2 => offset.is_multiple_of(24).then_some(offset / 24),
            3 => (offset.trailing_zeros() >= 5).then_some(offset >> 5),
            4 => offset.is_multiple_of(48).then_some(offset / 48),
            5 => (offset.trailing_zeros() >= 6).then_some(offset >> 6),
            6 => offset.is_multiple_of(80).then_some(offset / 80),
            7 => offset.is_multiple_of(96).then_some(offset / 96),
            8 => (offset.trailing_zeros() >= 7).then_some(offset >> 7),
            9 => offset.is_multiple_of(160).then_some(offset / 160),
            10 => offset.is_multiple_of(192).then_some(offset / 192),
            11 => (offset.trailing_zeros() >= 8).then_some(offset >> 8),
            12 => offset.is_multiple_of(320).then_some(offset / 320),
            13 => offset.is_multiple_of(384).then_some(offset / 384),
            14 => (offset.trailing_zeros() >= 9).then_some(offset >> 9),
            15 => offset.is_multiple_of(768).then_some(offset / 768),
            16 => (offset.trailing_zeros() >= 10).then_some(offset >> 10),
            17 => offset.is_multiple_of(1536).then_some(offset / 1536),
            18 => (offset.trailing_zeros() >= 11).then_some(offset >> 11),
            19 => offset.is_multiple_of(3072).then_some(offset / 3072),
            20 => (offset.trailing_zeros() >= 12).then_some(offset >> 12),
            21 => offset.is_multiple_of(6144).then_some(offset / 6144),
            22 => (offset.trailing_zeros() >= 13).then_some(offset >> 13),
            23 => offset.is_multiple_of(12288).then_some(offset / 12288),
            24 => (offset.trailing_zeros() >= 14).then_some(offset >> 14),
            25 => offset.is_multiple_of(24576).then_some(offset / 24576),
            26 => (offset.trailing_zeros() >= 15).then_some(offset >> 15),
            _ => None,
        }
    }
}

pub(crate) struct SizeClasses;

impl SizeClasses {
    pub(crate) const COUNT: usize = 27;
    pub(crate) const SMALL_MAX: usize = 32 * 1024;
    const MIN_ALIGNMENT: usize = 8;
    /// One entry per representable alignment power, from `2^0` up to and
    /// including `PAGE_SIZE`.
    const ALIGN_POWER_COUNT: usize = Self::align_power_count();
    /// The one hand-authored size-class table. The alignment remap is
    /// const-generated from this list.
    const SIZES: [usize; Self::COUNT] = [
        8, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 768, 1024, 1536, 2048,
        3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768,
    ];
    /// `ALIGNED_CLASS_BY_START[power][start]` is the smallest class index at
    /// or after `start` whose block size is a multiple of `2^power`. `SIZES`
    /// ends at `SMALL_MAX`, a multiple of every representable alignment, so
    /// every cell is a valid in-range index.
    const ALIGNED_CLASS_BY_START: [[usize; Self::COUNT]; Self::ALIGN_POWER_COUNT] =
        Self::build_aligned_class_map();
    /// `CLASS_FOR_SIZE[n]` is the smallest class index with `SIZES[i] >= n`
    /// for `n` in `1..=SMALL_MAX`. Index `0` is unused.
    const CLASS_FOR_SIZE: [u8; Self::SMALL_MAX + 1] = Self::build_class_for_size();

    const fn align_power_count() -> usize {
        let mut power = 0;
        let mut align = 1usize;
        while align < PAGE_SIZE {
            align <<= 1;
            power += 1;
        }
        power + 1
    }

    /// Builds [`Self::ALIGNED_CLASS_BY_START`] from [`Self::SIZES`] at compile time.
    ///
    /// `slice::get` is not yet usable from `const fn` on stable Rust, so this
    /// uses direct indexing. Every index is bounded by the loop condition
    /// immediately above it, so an out-of-bounds access can only ever be a
    /// compile-time evaluation error, never a runtime panic.
    #[allow(clippy::indexing_slicing)]
    const fn build_aligned_class_map() -> [[usize; Self::COUNT]; Self::ALIGN_POWER_COUNT] {
        let mut table = [[0usize; Self::COUNT]; Self::ALIGN_POWER_COUNT];

        let mut power = 0;
        while power < Self::ALIGN_POWER_COUNT {
            let align = 1usize << power;
            let mut start = 0;
            while start < Self::COUNT {
                let mut index = start;
                while index < Self::COUNT - 1 && Self::SIZES[index] % align != 0 {
                    index += 1;
                }
                // Every alignment power through PAGE_SIZE must be covered by
                // some size class at or after `start` (SIZES ends at SMALL_MAX).
                assert!(Self::SIZES[index] % align == 0);
                table[power][start] = index;
                start += 1;
            }
            power += 1;
        }

        table
    }

    /// Builds [`Self::CLASS_FOR_SIZE`] from [`Self::SIZES`] at compile time.
    ///
    /// The local table exists only for const evaluation into the static
    /// [`Self::CLASS_FOR_SIZE`]; `COUNT` fits in `u8`.
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::large_stack_arrays
    )]
    const fn build_class_for_size() -> [u8; Self::SMALL_MAX + 1] {
        const { assert!(SizeClasses::COUNT <= 255) };

        let mut table = [0u8; Self::SMALL_MAX + 1];
        let mut class = 0;
        let mut prev = 0usize;

        while class < Self::COUNT {
            let size = Self::SIZES[class];
            let mut n = prev + 1;
            while n <= size {
                table[n] = class as u8;
                n += 1;
            }
            prev = size;
            class += 1;
        }

        table
    }

    /// Map a normalized layout to a small size class, or `None` for large/over-aligned.
    ///
    /// Default-align (`align <= 8`) is the hot path: size bound + `CLASS_FOR_SIZE`
    /// only — no `PAGE_SIZE` check (align 8 is always ≤ page). Higher alignments
    /// take the align-remap table. Zero-size is already normalized by `LayoutSpec`.
    #[inline]
    pub(crate) fn id_for(spec: LayoutSpec) -> Option<SizeClassId> {
        let size = spec.size();
        let align = spec.align().get();
        let required = size.max(align);

        if align <= Self::MIN_ALIGNMENT {
            if required > Self::SMALL_MAX {
                return None;
            }

            // SAFETY: `required` is in `1..=SMALL_MAX`, so the table slot is
            // initialized; every stored class index is `< COUNT`.
            let index = usize::from(unsafe { *Self::CLASS_FOR_SIZE.get_unchecked(required) });
            // SAFETY: `index` came from `CLASS_FOR_SIZE` and is `< COUNT`.
            return Some(unsafe { SizeClassId::new_unchecked(index) });
        }

        if align > PAGE_SIZE || required > Self::SMALL_MAX {
            return None;
        }

        // SAFETY: `required` is in `1..=SMALL_MAX`, so the table slot is initialized.
        let lower_bound = usize::from(unsafe { *Self::CLASS_FOR_SIZE.get_unchecked(required) });
        Self::aligned_class_from(lower_bound, align)
    }

    /// Block size for a trusted [`SizeClassId`].
    pub(crate) fn block_size(id: SizeClassId) -> usize {
        // SAFETY: `SizeClassId` is only constructed for indexes in `SIZES`.
        unsafe { *Self::SIZES.get_unchecked(id.index()) }
    }

    #[cfg(test)]
    fn lower_bound_index(required: usize) -> Option<usize> {
        if required == 0 || required > Self::SMALL_MAX {
            return None;
        }

        // SAFETY: bounds checked above.
        Some(usize::from(unsafe {
            *Self::CLASS_FOR_SIZE.get_unchecked(required)
        }))
    }

    /// Smallest class index at or after `start` whose block size is a multiple
    /// of `align`, looked up in the const-generated align map.
    fn aligned_class_from(start: usize, align: usize) -> Option<SizeClassId> {
        debug_assert!(align.is_power_of_two());
        let align_power = usize::try_from(align.trailing_zeros()).ok()?;
        let index = *Self::ALIGNED_CLASS_BY_START.get(align_power)?.get(start)?;
        SizeClassId::new(index)
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use super::*;

    fn spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    #[test]
    fn size_classes_map_one_byte_to_eight() {
        let id = SizeClasses::id_for(spec(1, 1)).unwrap();

        assert_eq!(SizeClasses::block_size(id), 8);
    }

    #[test]
    fn size_classes_normalize_zero_size_via_layout_spec() {
        let id = SizeClasses::id_for(spec(0, 8)).unwrap();

        assert_eq!(SizeClasses::block_size(id), 8);
    }

    #[test]
    fn size_classes_map_exact_boundaries_to_themselves() {
        for &size in &SizeClasses::SIZES {
            let id = SizeClasses::id_for(spec(size, 1)).unwrap();

            assert_eq!(SizeClasses::block_size(id), size);
        }
    }

    #[test]
    fn size_classes_reject_larger_than_small_max() {
        assert!(SizeClasses::id_for(spec(SizeClasses::SMALL_MAX + 1, 1)).is_none());
    }

    #[test]
    fn size_classes_reject_over_page_alignment() {
        assert!(SizeClasses::id_for(spec(1, PAGE_SIZE * 2)).is_none());
    }

    #[test]
    fn size_classes_choose_naturally_aligned_block() {
        let id = SizeClasses::id_for(spec(17, 16)).unwrap();

        assert_eq!(SizeClasses::block_size(id), 32);
    }

    #[test]
    fn size_classes_match_linear_reference() {
        for size in 1..=SizeClasses::SMALL_MAX {
            for align in [
                1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
            ] {
                let class = SizeClasses::id_for(spec(size, align)).map(SizeClasses::block_size);
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

    #[test]
    fn aligned_class_map_matches_linear_oracle() {
        for power in 0..SizeClasses::ALIGN_POWER_COUNT {
            let align = 1_usize << power;

            for start in 0..SizeClasses::COUNT {
                let generated = SizeClasses::ALIGNED_CLASS_BY_START
                    .get(power)
                    .and_then(|row| row.get(start))
                    .copied();
                let reference = (start..SizeClasses::COUNT).find_map(|index| {
                    let size = *SizeClasses::SIZES.get(index)?;
                    size.is_multiple_of(align).then_some(index)
                });

                assert_eq!(generated, reference, "power={power} start={start}");
            }
        }
    }

    #[test]
    fn size_class_id_construction_rejects_out_of_bounds_index() {
        assert!(SizeClassId::new(SizeClasses::COUNT).is_none());
        assert!(SizeClassId::new(0).is_some());
        assert!(SizeClassId::new(SizeClasses::COUNT - 1).is_some());
    }

    #[test]
    fn block_index_offset_oracle_all_classes() {
        const RUN_SIZE: usize = 64 * 1024;

        for class_index in 0..SizeClasses::COUNT {
            let id = SizeClassId::new(class_index).unwrap();
            let size = SizeClasses::block_size(id);
            let capacity = RUN_SIZE / size;

            for block in 0..capacity {
                let offset = block * size;
                assert_eq!(id.block_index_from_offset(offset), Some(block));
                assert_eq!(id.block_offset(block), offset);
            }

            if size > 1 {
                for block in 0..capacity {
                    let base = block * size;
                    for interior in 1..size {
                        assert_eq!(id.block_index_from_offset(base + interior), None);
                    }
                }
            }
        }
    }
}
