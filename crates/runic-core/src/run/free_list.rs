use core::ptr::NonNull;

#[repr(C)]
struct FreeNode {
    next: Option<NonNull<Self>>,
}

pub(crate) struct FreeList {
    head: Option<NonNull<FreeNode>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FreeBlock {
    ptr: NonNull<u8>,
}

impl FreeBlock {
    pub(crate) unsafe fn new_unchecked(ptr: NonNull<u8>) -> Self {
        Self { ptr }
    }

    pub(crate) const fn ptr(self) -> NonNull<u8> {
        self.ptr
    }
}

impl FreeList {
    pub(crate) const fn new() -> Self {
        Self { head: None }
    }

    pub(crate) unsafe fn push(&mut self, block: FreeBlock) {
        let node = block.ptr().cast::<FreeNode>();

        // SAFETY: caller guarantees block points to writable memory large enough to hold FreeNode.
        unsafe { node.as_ptr().write(FreeNode { next: self.head }) };

        self.head = Some(node);
    }

    pub(crate) fn pop(&mut self) -> Option<FreeBlock> {
        let head = self.head?;

        // SAFETY: head was previously written by push as a valid FreeNode.
        unsafe { self.head = head.as_ref().next };

        // SAFETY: popped nodes were inserted as FreeBlock pointers.
        Some(unsafe { FreeBlock::new_unchecked(head.cast()) })
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;

    use super::*;

    #[repr(align(16))]
    struct Blocks {
        bytes: [u8; 64],
    }

    impl Blocks {
        const fn new() -> Self {
            Self { bytes: [0; 64] }
        }

        fn ptr_at(&mut self, offset: usize) -> NonNull<u8> {
            let ptr = unsafe { self.bytes.as_mut_ptr().add(offset) };
            unsafe { NonNull::new_unchecked(ptr) }
        }
    }

    #[test]
    fn free_list_new_pop_returns_none() {
        let mut list = FreeList::new();

        assert!(list.pop().is_none());
    }

    #[test]
    fn free_list_returns_single_pushed_block() {
        let mut blocks = Blocks::new();
        let ptr = blocks.ptr_at(0);
        let mut list = FreeList::new();

        unsafe { list.push(FreeBlock::new_unchecked(ptr)) };

        assert_eq!(list.pop().map(FreeBlock::ptr), Some(ptr));
        assert!(list.pop().is_none());
    }

    #[test]
    fn free_list_pops_in_lifo_order() {
        let mut blocks = Blocks::new();
        let first = blocks.ptr_at(0);
        let second = blocks.ptr_at(16);
        let third = blocks.ptr_at(32);
        let mut list = FreeList::new();

        unsafe {
            list.push(FreeBlock::new_unchecked(first));
            list.push(FreeBlock::new_unchecked(second));
            list.push(FreeBlock::new_unchecked(third));
        }

        assert_eq!(list.pop().map(FreeBlock::ptr), Some(third));
        assert_eq!(list.pop().map(FreeBlock::ptr), Some(second));
        assert_eq!(list.pop().map(FreeBlock::ptr), Some(first));
        assert!(list.pop().is_none());
    }
}
