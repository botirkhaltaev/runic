//! Queues. Each atomicity is its own type. The lock-free CAS is [`stack`].
//!
//! [`Queue`] is owner-exclusive (`&mut self`, [`Cell`] links, `push` / `pop`).
//! [`Mpsc`] is multi-producer single-consumer (`push` / `drain`). [`Mpmc`] is
//! multi-producer multi-consumer (`push` / `pop`). `drain` is MPSC only: one consumer takes the chain.

mod mpmc;
mod mpsc;
pub(crate) mod stack;

use core::{cell::Cell, marker::PhantomData, ptr::NonNull};

pub(crate) use mpmc::Mpmc;
pub(crate) use mpsc::{Drain, Mpsc};

/// Intrusive `next` plus membership. `linked` distinguishes a tail (`next` is
/// empty) from a node that is not on the queue.
pub(crate) struct Link<T> {
    next: Cell<Option<NonNull<T>>>,
    linked: Cell<bool>,
}

impl<T> Link<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: Cell::new(None),
            linked: Cell::new(false),
        }
    }

    #[inline]
    pub(crate) fn is_linked(&self) -> bool {
        self.linked.get()
    }

    fn clear(&self) {
        self.next.set(None);
        self.linked.set(false);
    }
}

/// Types that embed a [`Link`] for [`Queue`].
pub(crate) trait Linked: Sized {
    fn links(&self) -> &Link<Self>;
}

/// Owner-exclusive queue of nodes borrowed for `'a`.
///
/// One thread holds `&mut self`. `push` / `pop` are the front of the chain
/// (newest first). Links are [`Cell`]s, not atomics.
pub(crate) struct Queue<'a, T: Linked> {
    head: Option<NonNull<T>>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> Queue<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            marker: PhantomData,
        }
    }

    /// Link `node` in front of the head. `node` must not already be queued.
    pub(crate) fn push(&mut self, node: &'a T) {
        debug_assert!(!node.links().is_linked());
        let ptr = NonNull::from(node);
        node.links().next.set(self.head);
        node.links().linked.set(true);
        self.head = Some(ptr);
    }

    /// Pop the newest node.
    pub(crate) fn pop(&mut self) -> Option<&'a T> {
        let ptr = self.head?;
        // SAFETY: `push` stored this pointer from `&'a T`.
        let node = unsafe { ptr.as_ref() };
        self.head = node.links().next.get();
        node.links().clear();
        Some(node)
    }
}

// SAFETY: `head` points at an arena slot of `T`. Mutation is `&mut self` only.
// `NonNull<T>` does not carry `T: Send`, so the queue states it here.
unsafe impl<T: Linked + Send> Send for Queue<'_, T> {}
// SAFETY: sharing `&Queue` does not mutate `head`. Node links are the owner's.
unsafe impl<T: Linked + Sync> Sync for Queue<'_, T> {}

#[cfg(test)]
mod tests {
    use core::ptr;

    use super::*;

    struct TestNode {
        link: Link<TestNode>,
    }

    impl TestNode {
        fn new() -> Self {
            Self { link: Link::new() }
        }
    }

    impl Linked for TestNode {
        fn links(&self) -> &Link<Self> {
            &self.link
        }
    }

    #[test]
    fn push_then_pop_is_lifo() {
        let mut queue = Queue::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        queue.push(&older);
        queue.push(&newer);
        assert!(ptr::eq(
            ptr::from_ref(queue.pop().unwrap()),
            ptr::from_ref(&newer)
        ));
        assert!(ptr::eq(
            ptr::from_ref(queue.pop().unwrap()),
            ptr::from_ref(&older)
        ));
        assert!(queue.pop().is_none());
        assert!(!older.link.is_linked());
        assert!(!newer.link.is_linked());
    }
}
