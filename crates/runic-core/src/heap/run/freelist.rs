//! Owner-exclusive stack of free run blocks.
//!
//! The head is a payload address, or [`END`] when the stack is empty. Each free
//! block stores the next address in its first word. That word belongs to the
//! caller again after [`Freelist::pop`].

use core::{cell::Cell, mem::size_of, ptr::NonNull};

#[cfg(feature = "hardened")]
use crate::allocator::Allocator;

/// Empty head and end-of-stack link. A payload address is never 0.
const END: usize = 0;

/// Hardened link word: a 16-bit tag over the top of the address, then the
/// low 48 bits mixed with the cookie. A smashed word fails the tag.
#[cfg(feature = "hardened")]
const TAG: u64 = 0xA5A5;
#[cfg(feature = "hardened")]
const LOW: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Head of the free-block stack.
///
/// Fast stores the next address raw, so this stays one word. Hardened keeps a
/// per-run cookie beside the head and encodes every link with it. Callers do
/// not pass the cookie: [`Self::load`] and [`Self::store`] are the only place
/// the build differs.
pub(crate) struct Freelist {
    head: Cell<usize>,
    #[cfg(feature = "hardened")]
    cookie: u64,
}

impl Freelist {
    pub(super) fn new(base: usize) -> Self {
        #[cfg(not(feature = "hardened"))]
        let _ = base;
        Self {
            head: Cell::new(END),
            #[cfg(feature = "hardened")]
            cookie: Self::word(base).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// Pop the head block. `None` when the stack is empty.
    #[inline]
    pub(super) fn pop(&self) -> Option<NonNull<u8>> {
        let raw = self.head.get();
        if raw == END {
            return None;
        }
        let block = NonNull::new(core::ptr::without_provenance_mut(raw))?;
        let next = self.load(block);
        self.head.set(next);
        Some(block)
    }

    /// `true` when [`Self::pop`] would return `None`.
    #[inline]
    #[cfg(feature = "hardened")]
    pub(super) fn is_empty(&self) -> bool {
        self.head.get() == END
    }

    #[cfg(feature = "hardened")]
    #[inline]
    pub(super) fn cookie(&self) -> u64 {
        self.cookie
    }

    /// Push `block` in front of the head. `block` is not already on this stack.
    #[inline]
    pub(super) fn push(&self, block: NonNull<u8>) {
        self.store(block, self.head.get());
        self.head.set(block.as_ptr().addr());
    }

    /// Drop every block. The payload links are left behind.
    pub(super) fn clear(&self) {
        self.head.set(END);
    }

    /// Write `block`'s link word. Remote chains and the owner stack share this word.
    #[inline]
    pub(super) fn link(&self, block: NonNull<u8>, next: Option<NonNull<u8>>) {
        let addr = next.map_or(END, |next| next.as_ptr().addr());
        self.store(block, addr);
    }

    /// Read `block`'s link word. `None` is the tail.
    #[inline]
    pub(super) fn next(&self, block: NonNull<u8>) -> Option<NonNull<u8>> {
        NonNull::new(core::ptr::without_provenance_mut(self.load(block)))
    }

    /// Prepend the chain `head`…`tail` in front of this stack. Owner only.
    ///
    /// `tail`'s link becomes the previous head. Pop order is `head` first.
    /// Hardened delays each block instead of splicing the chain.
    #[cfg(not(feature = "hardened"))]
    #[inline]
    pub(super) fn splice(&self, head: NonNull<u8>, tail: NonNull<u8>) {
        self.store(tail, self.head.get());
        self.head.set(head.as_ptr().addr());
    }

    /// Link `count` adjacent blocks of `stride` bytes, starting at `first`, in
    /// front of the current head.
    ///
    /// Pop order is `first`, then each following block, then the previous head.
    /// `count` is non-zero, `stride >= size_of::<usize>()`, and `first` addresses
    /// `count` blocks owned by the caller and not already on this stack.
    pub(super) fn push_contiguous(&self, first: NonNull<u8>, count: usize, stride: usize) {
        debug_assert!(count > 0);
        debug_assert!(stride >= size_of::<usize>());
        if count == 0 {
            return;
        }
        let mut block = first;
        for _ in 1..count {
            // SAFETY: `first` starts `count` in-bounds blocks of `stride` bytes,
            // so each step stays inside that span and never reaches address 0.
            let next = unsafe { block.byte_add(stride) };
            self.store(block, next.as_ptr().addr());
            block = next;
        }
        self.store(block, self.head.get());
        self.head.set(first.as_ptr().addr());
    }

    /// Abort path for Safe free: `Ok` when `block` is not on this stack.
    ///
    /// The first word of a listed block is [`END`] or another block. Any other
    /// word means `block` is not listed, so the walk is skipped. A chain longer
    /// than `limit` is corrupt.
    #[cfg(feature = "safe")]
    pub(super) fn ensure_absent(
        &self,
        block: NonNull<u8>,
        limit: usize,
        is_block: impl Fn(NonNull<u8>) -> bool,
    ) -> Result<(), FreelistError> {
        let next = Self::read(block);
        let looks_linked =
            NonNull::new(core::ptr::without_provenance_mut::<u8>(next)).is_none_or(&is_block);
        if !looks_linked {
            return Ok(());
        }
        let mut raw = self.head.get();
        for _ in 0..limit {
            if raw == END {
                return Ok(());
            }
            let Some(link) = NonNull::new(core::ptr::without_provenance_mut(raw)) else {
                return Err(FreelistError::Corrupt);
            };
            if link == block {
                return Err(FreelistError::Listed);
            }
            raw = Self::read(link);
        }
        if raw == END {
            Ok(())
        } else {
            Err(FreelistError::Corrupt)
        }
    }

    /// Decode `block`'s link word. Hardened aborts on a bad tag.
    #[inline]
    fn load(&self, block: NonNull<u8>) -> usize {
        let word = Self::read(block);
        #[cfg(feature = "hardened")]
        {
            let word = Self::word(word);
            if word >> 48 != self.tag() {
                Allocator::abort();
            }
            Self::addr((word ^ self.cookie) & LOW)
        }
        #[cfg(not(feature = "hardened"))]
        {
            let _ = self;
            word
        }
    }

    /// Encode `next` into `block`'s link word.
    #[inline]
    fn store(&self, block: NonNull<u8>, next: usize) {
        #[cfg(feature = "hardened")]
        let next = Self::addr(self.tag() << 48 | (Self::word(next) ^ self.cookie) & LOW);
        #[cfg(not(feature = "hardened"))]
        let _ = self;
        Self::write(block, next);
    }

    #[cfg(feature = "hardened")]
    #[inline]
    fn tag(&self) -> u64 {
        (TAG ^ self.cookie.rotate_right(17)) & 0xFFFF
    }

    #[cfg(feature = "hardened")]
    #[inline]
    fn word(addr: usize) -> u64 {
        u64::try_from(addr).unwrap_or_else(|_| Allocator::abort())
    }

    #[cfg(feature = "hardened")]
    #[inline]
    fn addr(word: u64) -> usize {
        usize::try_from(word).unwrap_or_else(|_| Allocator::abort())
    }

    #[inline]
    fn read(block: NonNull<u8>) -> usize {
        // SAFETY: the caller passes a free block of this run. Its first word is the link.
        unsafe { block.cast::<usize>().as_ptr().read() }
    }

    #[inline]
    fn write(block: NonNull<u8>, next: usize) {
        // SAFETY: the caller passes a free block of this run. Its first word is the link.
        unsafe { block.cast::<usize>().as_ptr().write(next) }
    }
}

/// `block` is already linked, or the chain does not end within the block limit.
#[cfg(feature = "safe")]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FreelistError {
    Listed,
    Corrupt,
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;
    use core::ptr::{self, NonNull};

    use super::Freelist;

    fn addr_of(slot: &usize) -> usize {
        ptr::from_ref(slot).cast::<u8>().addr()
    }

    fn at(addr: usize) -> NonNull<u8> {
        NonNull::new(ptr::without_provenance_mut(addr)).unwrap()
    }

    #[test]
    fn push_then_pop_is_lifo() {
        let slots = [0_usize; 2];
        let first = addr_of(&slots[0]);
        let second = addr_of(&slots[1]);
        let list = Freelist::new(first);

        assert!(list.pop().is_none());
        list.push(at(first));
        list.push(at(second));
        assert_eq!(list.pop(), Some(at(second)));
        assert_eq!(list.pop(), Some(at(first)));
        assert!(list.pop().is_none());
    }

    #[test]
    fn push_contiguous_links_low_to_high_then_the_old_head() {
        let fresh = [0_usize; 3];
        let old = [0_usize; 1];
        let fresh_addrs = [addr_of(&fresh[0]), addr_of(&fresh[1]), addr_of(&fresh[2])];
        let old_addr = addr_of(&old[0]);
        let list = Freelist::new(old_addr);

        list.push(at(old_addr));
        list.push_contiguous(at(fresh_addrs[0]), 3, size_of::<usize>());
        assert_eq!(list.pop(), Some(at(fresh_addrs[0])));
        assert_eq!(list.pop(), Some(at(fresh_addrs[1])));
        assert_eq!(list.pop(), Some(at(fresh_addrs[2])));
        assert_eq!(list.pop(), Some(at(old_addr)));
        assert!(list.pop().is_none());
    }

    #[test]
    fn clear_drops_the_head() {
        let slot = [0_usize; 1];
        let list = Freelist::new(addr_of(&slot[0]));
        list.push(at(addr_of(&slot[0])));
        list.clear();
        assert!(list.pop().is_none());
    }

    #[cfg(feature = "safe")]
    #[test]
    fn ensure_absent_walks_only_words_that_look_like_links() {
        use super::FreelistError;

        let listed_slot = [0_usize; 1];
        let mut live_word = 1_usize;
        let listed = addr_of(&listed_slot[0]);
        let live = addr_of(&live_word);
        let list = Freelist::new(listed);
        let is_block = |ptr: NonNull<u8>| ptr.addr().get() == listed || ptr.addr().get() == live;

        list.push(at(listed));
        assert_eq!(
            list.ensure_absent(at(listed), 2, is_block),
            Err(FreelistError::Listed)
        );

        assert_eq!(live_word, 1);
        assert_eq!(list.ensure_absent(at(live), 2, is_block), Ok(()));

        live_word = 0;
        assert_eq!(list.ensure_absent(at(live), 2, is_block), Ok(()));
        assert_eq!(live_word, 0);
        assert_eq!(
            list.ensure_absent(at(live), 0, is_block),
            Err(FreelistError::Corrupt)
        );
    }
}
