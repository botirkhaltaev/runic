use core::ptr::NonNull;

#[repr(C)]
struct FreeNode {
    next: Option<NonNull<Self>>,
}

pub(crate) struct FreeList {
    head: Option<NonNull<FreeNode>>,
}

impl FreeList {
    pub(crate) const fn new() -> Self {
        Self { head: None }
    }

    pub(crate) unsafe fn push(&mut self, ptr: NonNull<u8>) {
        let node = ptr.cast::<FreeNode>();

        // SAFETY: caller guarantees ptr is writable memory large enough to hold FreeNode.
        unsafe { node.as_ptr().write(FreeNode { next: self.head }) };

        self.head = Some(node);
    }

    pub(crate) fn pop(&mut self) -> Option<NonNull<u8>> {
        let head = self.head?;

        // SAFETY: head was previously written by push as a valid FreeNode.
        unsafe { self.head = head.as_ref().next };

        Some(head.cast())
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;

    use super::*;

    #[repr(align(16))]
    struct Blocks([u8; 64]);

    impl Blocks {
        fn ptr_at(&mut self, offset: usize) -> NonNull<u8> {
            let ptr = unsafe { self.0.as_mut_ptr().add(offset) };
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
        let mut blocks = Blocks([0; 64]);
        let ptr = blocks.ptr_at(0);
        let mut list = FreeList::new();

        unsafe { list.push(ptr) };

        assert_eq!(list.pop(), Some(ptr));
        assert!(list.pop().is_none());
    }

    #[test]
    fn free_list_pops_in_lifo_order() {
        let mut blocks = Blocks([0; 64]);
        let first = blocks.ptr_at(0);
        let second = blocks.ptr_at(16);
        let third = blocks.ptr_at(32);
        let mut list = FreeList::new();

        unsafe {
            list.push(first);
            list.push(second);
            list.push(third);
        }

        assert_eq!(list.pop(), Some(third));
        assert_eq!(list.pop(), Some(second));
        assert_eq!(list.pop(), Some(first));
        assert!(list.pop().is_none());
    }
}
