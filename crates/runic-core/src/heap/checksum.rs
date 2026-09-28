//! Digest of header fields fixed after publish. `hardened` only.

/// Three header words mixed into one. A run digests base, class, and owner.
/// An extent digests owner, mapping base, and mapping length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Checksum(u64);

impl Checksum {
    pub(crate) fn of(parts: [usize; 3]) -> Self {
        let [a, b, c] = parts.map(Self::word);
        let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        x ^= c;
        x ^= x >> 33;
        Self(x.wrapping_mul(0xFF51_AFD7_ED55_8CCD))
    }

    /// A digest no header produces. Tests damage a header with it.
    #[cfg(test)]
    pub(crate) const fn damaged() -> Self {
        Self(0)
    }

    fn word(part: usize) -> u64 {
        u64::try_from(part).unwrap_or_else(|_| crate::allocator::Allocator::abort())
    }
}
