use crate::{layout::LayoutSpec, memory::PAGE_SIZE};

/// Trusted size class (index into [`SizeClasses::SIZES`]).
///
/// Only [`SizeClasses`] can construct this type, and only for in-range indexes,
/// so hot paths may treat `index()` as a trusted subscript into size-class
/// arrays of length [`SizeClasses::COUNT`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SizeClass {
    index: usize,
}

impl SizeClass {
    pub(crate) const fn index(self) -> usize {
        self.index
    }

    /// Returns a class for `index`, or `None` when out of range for `SIZES`.
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

    /// Byte size of blocks in this class.
    #[inline]
    pub(crate) fn size(self) -> usize {
        // SAFETY: `SizeClass` is only constructed for indexes in `SIZES`.
        unsafe { *SizeClasses::SIZES.get_unchecked(self.index()) }
    }
}

pub(crate) struct SizeClasses;

/// One hand-authored size list. Indexes, `SIZES`, `COUNT`, and `SizeClass::index_of`
/// arms are generated together so they cannot drift.
macro_rules! define_size_classes {
    ($($size:literal),+ $(,)?) => {
        define_size_classes!(@zip
            [0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26]
            [$($size),+]
        );
    };

    (@zip [$i:literal $($irest:literal)*] [$size:literal $(, $srest:literal)*]) => {
        define_size_classes!(@go [($i, $size)] [$($irest)*] [$($srest),*]);
    };

    (@go [$(($i:literal, $size:literal))+] [$ni:literal $($irest:literal)*] [$nsize:literal $(, $srest:literal)*]) => {
        define_size_classes!(@go [$(($i, $size))+ ($ni, $nsize)] [$($irest)*] [$($srest),*]);
    };

    (@go [$(($i:literal, $size:literal))+] [$($irest:literal)+] []) => {
        compile_error!(
            "define_size_classes!: more index slots than sizes; extend the list or trim indexes"
        );
    };

    (@go [$(($i:literal, $size:literal))+] [] [$($srest:literal),+]) => {
        compile_error!(
            "define_size_classes!: more sizes than index slots; extend the index pool"
        );
    };

    (@go [$(($i:literal, $size:literal))+] [] []) => {
        impl SizeClass {
            /// Block index of a payload offset for this class; rejects non-boundary offsets.
            #[inline]
            pub(crate) fn index_of(self, offset: usize) -> Option<usize> {
                // SAFETY: `SizeClass` is only minted for indexes in `0..COUNT`.
                let shift = unsafe { *SizeClasses::SHIFTS.get_unchecked(self.index()) };
                if shift != 0 {
                    let mask = (1_usize << shift) - 1;
                    (offset & mask == 0).then_some(offset >> shift)
                } else {
                    // Separate method so the power-of-two path stays a shift-table load
                    // after inlining; the match is large enough to be its own unit.
                    Self::index_match(self.index(), offset)
                }
            }

            #[inline]
            fn index_match(index: usize, offset: usize) -> Option<usize> {
                match index {
                    $(
                        $i => offset.is_multiple_of($size).then_some(offset / $size),
                    )+
                    // SAFETY: `SizeClass` is only minted for indexes in `0..COUNT`.
                    _ => unsafe { core::hint::unreachable_unchecked() },
                }
            }
        }

        impl SizeClasses {
            /// The one hand-authored size-class declaration. Constant-divisor
            /// indexing and derived lookup tables are generated from this list.
            pub(crate) const SIZES: [usize; [$($size),+].len()] = [$($size),+];
            pub(crate) const COUNT: usize = Self::SIZES.len();
            /// `trailing_zeros(size)` for power-of-two classes; `0` means use the
            /// const-divisor match in [`SizeClass::index_of`] (minimum power-of-two class is 8).
            #[allow(clippy::indexing_slicing)]
            const SHIFTS: [u32; Self::COUNT] = {
                let mut table = [0u32; Self::COUNT];
                let sizes = Self::SIZES;
                let mut i = 0;
                while i < Self::COUNT {
                    let size = sizes[i];
                    if size.is_power_of_two() {
                        table[i] = size.trailing_zeros();
                    }
                    i += 1;
                }
                table
            };
        }
    };
}

define_size_classes! {
    8usize, 16usize, 24usize, 32usize, 48usize, 64usize, 80usize, 96usize, 128usize, 160usize,
    192usize, 256usize, 320usize, 384usize, 512usize, 768usize, 1024usize, 1536usize, 2048usize,
    3072usize, 4096usize, 6144usize, 8192usize, 12288usize, 16384usize, 24576usize, 32768usize,
}

impl SizeClasses {
    pub(crate) const SMALL_MAX: usize = 32 * 1024;
    const MIN_ALIGNMENT: usize = 8;
    /// One entry per representable alignment power, from `2^0` up to and
    /// including `PAGE_SIZE`.
    const ALIGN_POWER_COUNT: usize = Self::align_power_count();
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
    pub(crate) fn class_for(spec: LayoutSpec) -> Option<SizeClass> {
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
            return Some(unsafe { SizeClass::new_unchecked(index) });
        }

        if align > PAGE_SIZE || required > Self::SMALL_MAX {
            return None;
        }

        // SAFETY: `required` is in `1..=SMALL_MAX`, so the table slot is initialized.
        let lower_bound = usize::from(unsafe { *Self::CLASS_FOR_SIZE.get_unchecked(required) });
        Self::aligned_class_from(lower_bound, align)
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
    fn aligned_class_from(start: usize, align: usize) -> Option<SizeClass> {
        debug_assert!(align.is_power_of_two());
        let align_power = usize::try_from(align.trailing_zeros()).ok()?;
        let index = *Self::ALIGNED_CLASS_BY_START.get(align_power)?.get(start)?;
        SizeClass::new(index)
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
        let class = SizeClasses::class_for(spec(1, 1)).unwrap();

        assert_eq!(class.size(), 8);
    }

    #[test]
    fn size_classes_normalize_zero_size_via_layout_spec() {
        let class = SizeClasses::class_for(spec(0, 8)).unwrap();

        assert_eq!(class.size(), 8);
    }

    #[test]
    fn size_classes_map_exact_boundaries_to_themselves() {
        for &size in &SizeClasses::SIZES {
            let class = SizeClasses::class_for(spec(size, 1)).unwrap();

            assert_eq!(class.size(), size);
        }
    }

    #[test]
    fn size_classes_reject_larger_than_small_max() {
        assert!(SizeClasses::class_for(spec(SizeClasses::SMALL_MAX + 1, 1)).is_none());
    }

    #[test]
    fn size_classes_reject_over_page_alignment() {
        assert!(SizeClasses::class_for(spec(1, PAGE_SIZE * 2)).is_none());
    }

    #[test]
    fn size_classes_choose_naturally_aligned_block() {
        let class = SizeClasses::class_for(spec(17, 16)).unwrap();

        assert_eq!(class.size(), 32);
    }

    #[test]
    fn size_classes_match_linear_reference() {
        for size in 1..=SizeClasses::SMALL_MAX {
            for align in [
                1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
            ] {
                let class = SizeClasses::class_for(spec(size, align)).map(SizeClass::size);
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
    fn size_class_construction_rejects_out_of_bounds_index() {
        assert!(SizeClass::new(SizeClasses::COUNT).is_none());
        assert!(SizeClass::new(0).is_some());
        assert!(SizeClass::new(SizeClasses::COUNT - 1).is_some());
    }

    #[test]
    fn index_of_matches_linear_oracle_for_all_classes() {
        for class_index in 0..SizeClasses::COUNT {
            let class = SizeClass::new(class_index).unwrap();
            let size = class.size();

            for offset in 0..=size * 2 {
                let reference = offset
                    .is_multiple_of(size)
                    .then_some(offset.checked_div(size).unwrap());
                assert_eq!(
                    class.index_of(offset),
                    reference,
                    "class={class_index} size={size} offset={offset}"
                );
            }
        }
    }
}
