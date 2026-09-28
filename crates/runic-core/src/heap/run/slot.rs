//! One size-class slot and the canary in its last word.
//!
//! Fast has no canary, so [`Slot::mark`] and [`Slot::expect`] do nothing.
//! Hardened mixes the run cookie into the word: a stale or foreign word does
//! not match.

use core::ptr::NonNull;

#[cfg(feature = "hardened")]
use core::mem::size_of;

#[cfg(feature = "hardened")]
use crate::allocator::Allocator;

/// One size-class slot, addressed by its block.
pub(super) struct Slot {
    block: NonNull<u8>,
    stride: usize,
    #[cfg(feature = "hardened")]
    cookie: u64,
}

/// The canary written in the last word. `Live` while the user holds the block,
/// `Free` on the freelist or in the delay, `Claim` on a remote chain.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Canary {
    Live,
    Free,
    Claim,
}

#[cfg(feature = "hardened")]
impl Canary {
    fn salt(self) -> u64 {
        match self {
            Self::Live => 0xC4A1_57A1_7E11_0001,
            Self::Free => 0xF4EE_F4EE_F4EE_0002,
            Self::Claim => 0xC1A1_C1A1_C1A1_0003,
        }
    }

    /// `Free` does not mix the address, so the word still matches after the
    /// block moves between the freelist and the delay.
    fn word(self, block: NonNull<u8>, cookie: u64) -> usize {
        let addr = match self {
            Self::Live | Self::Claim => Self::widen(block.as_ptr().addr()),
            Self::Free => 0,
        };
        usize::try_from(cookie ^ addr ^ self.salt()).unwrap_or_else(|_| Allocator::abort())
    }

    fn widen(addr: usize) -> u64 {
        u64::try_from(addr).unwrap_or_else(|_| Allocator::abort())
    }
}

impl Slot {
    pub(super) fn new(
        block: NonNull<u8>,
        stride: usize,
        #[cfg(feature = "hardened")] cookie: u64,
    ) -> Self {
        Self {
            block,
            stride,
            #[cfg(feature = "hardened")]
            cookie,
        }
    }

    /// Write `canary` into the last word.
    #[inline]
    pub(super) fn mark(&self, canary: Canary) {
        #[cfg(feature = "hardened")]
        if let Some(last) = self.last(canary) {
            // SAFETY: the canary word is inside this slot, see `last`.
            unsafe { last.write(canary.word(self.block, self.cookie)) }
        }
        #[cfg(not(feature = "hardened"))]
        let _ = (self.block, self.stride, canary);
    }

    /// Abort unless the last word still says `canary`.
    #[inline]
    pub(super) fn expect(&self, canary: Canary) {
        #[cfg(feature = "hardened")]
        if let Some(last) = self.last(canary) {
            // SAFETY: the canary word is inside this slot, see `last`.
            if unsafe { last.read() } != canary.word(self.block, self.cookie) {
                Allocator::abort();
            }
        }
        #[cfg(not(feature = "hardened"))]
        let _ = (self.block, self.stride, canary);
    }

    /// The canary word, or `None` when this slot has no room for one besides
    /// the link. The 8-byte slot's only word is the link, so a claim marker
    /// there would not survive [`super::Run::push`]. A second claim still
    /// aborts because the word is no longer the live canary.
    #[cfg(feature = "hardened")]
    #[inline]
    fn last(&self, canary: Canary) -> Option<NonNull<usize>> {
        if canary == Canary::Claim && self.stride == size_of::<usize>() {
            return None;
        }
        // SAFETY: `block` is a slot of this run and `stride` is at least one
        // word, so the last word is inside the slot and belongs to the allocator.
        Some(unsafe { self.block.byte_add(self.stride - size_of::<usize>()).cast() })
    }
}
