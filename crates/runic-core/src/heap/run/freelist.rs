//! Owner-exclusive stack of free run blocks.
//!
//! The head is a payload address, or [`END`] when the stack is empty. Each free
//! block stores the next address in its first word. That word belongs to the
//! caller again after [`Freelist::pop`].

use core::{cell::Cell, mem::size_of, ptr::NonNull};

/// Empty head and end-of-stack link. A payload address is never 0.
const END: usize = 0;

/// Head of the free-block stack. `repr(transparent)` keeps it one word in `RunState`.
#[repr(transparent)]
pub(super) struct Freelist {
    head: Cell<usize>,
}

impl Freelist {
    pub(super) const fn new() -> Self {
        Self {
            head: Cell::new(END),
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
        self.head.set(Self::read(block));
        Some(block)
    }

    /// Push `block` in front of the head. `block` is not already on this stack.
    #[inline]
    pub(super) fn push(&self, block: NonNull<u8>) {
        Self::write(block, self.head.get());
        self.head.set(block.as_ptr().addr());
    }

    /// Drop every block. The payload links are left behind.
    pub(super) fn clear(&self) {
        self.head.set(END);
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
            Self::write(block, next.as_ptr().addr());
            block = next;
        }
        Self::write(block, self.head.get());
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
        let list = Freelist::new();

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
        let list = Freelist::new();

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
        let list = Freelist::new();
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
        let list = Freelist::new();
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
