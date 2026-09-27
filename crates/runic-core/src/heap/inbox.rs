//! Owner inbox: an [`Mpsc`] queue that holds each run or extent at most once.
//!
//! [`Inbox::enqueue`] CASes the link from idle to pushing, then links the node.
//! A loser sees a non-idle link and leaves it. The owner [`Inbox::drain`]s
//! (reading `next` before the caller runs) and `accept` stores idle.

use core::ptr;

use super::queue::{
    Drain, Mpsc,
    stack::{self, Linked},
};

/// Inbox link embedded on `Run` / `Extent`. One atomic `next`.
///
/// Idle is [`Link::IDLE`], never null. Null is only a real tail's `next`.
/// [`Link::PUSHING`] covers the window between the idle CAS and [`Mpsc::push`].
/// Run and extent pointers are at least pointer-aligned, so the sentinels are
/// never real nodes.
pub(crate) struct Link<T> {
    stack: stack::Link<T>,
}

impl<T> Link<T> {
    const IDLE: usize = 1;
    const PUSHING: usize = 2;

    pub(crate) const fn new() -> Self {
        Self {
            stack: stack::Link::dangling(Self::IDLE),
        }
    }

    /// Idle means the node is on no inbox and may be enqueued.
    #[inline]
    pub(crate) fn is_idle(&self) -> bool {
        self.stack.load().addr() == Self::IDLE
    }

    /// Idle → pushing. `true` when this call won the CAS and owns the enqueue.
    ///
    /// Active freers take a lease before calling this for a new win so close
    /// cannot observe a non-idle link without a subsequent [`Mpsc::push`].
    #[inline]
    pub(crate) fn reserve(&self) -> bool {
        let idle = ptr::without_provenance_mut(Self::IDLE);
        let pushing = ptr::without_provenance_mut(Self::PUSHING);
        self.stack.cas(idle, pushing).is_ok()
    }

    /// Store idle. Owner-only, after [`Mpsc::drain`] has loaded `next`.
    #[inline]
    pub(crate) fn idle(&self) {
        self.stack.store(ptr::without_provenance_mut(Self::IDLE));
    }
}

/// Types that embed a [`Link`] for coalesced inbox membership.
pub(crate) trait Node: Sized {
    fn link(&self) -> &Link<Self>;
}

impl<T: Node> Linked for T {
    fn links(&self) -> &stack::Link<Self> {
        &self.link().stack
    }
}

/// Remote-free inbox of distinct runs or extents. Single-consumer `drain`.
pub(crate) struct Inbox<'a, T: Node> {
    queue: Mpsc<'a, T>,
}

impl<'a, T: Node> Inbox<'a, T> {
    pub(crate) const fn new() -> Self {
        Self { queue: Mpsc::new() }
    }

    /// Link `node` when it is idle. `true` when this call pushed it.
    pub(crate) fn enqueue(&self, node: &'a T) -> bool {
        if !node.link().reserve() {
            return false;
        }
        self.queue.push(node);
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Take every enqueued node. `accept` stores idle after this iterator has read `next`.
    pub(crate) fn drain(&self) -> Drain<'a, T> {
        self.queue.drain()
    }

    /// Drain until empty. `drain` has already read `next`, so `accept` may
    /// [`Self::enqueue`] this inbox again.
    pub(crate) fn flush<E>(
        &self,
        mut accept: impl FnMut(&'a T, &Self) -> Result<(), E>,
    ) -> Result<(), E> {
        while !self.is_empty() {
            for node in self.drain() {
                accept(node, self)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestNode {
        link: Link<TestNode>,
    }

    impl TestNode {
        fn new() -> Self {
            Self { link: Link::new() }
        }
    }

    impl Node for TestNode {
        fn link(&self) -> &Link<Self> {
            &self.link
        }
    }

    #[test]
    fn enqueue_pushes_once_until_cleared() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.enqueue(&node));
        assert!(!inbox.enqueue(&node));
        assert!(!inbox.enqueue(&node));
        assert_eq!(inbox.drain().count(), 1);
        assert!(inbox.is_empty());
    }

    #[test]
    fn enqueue_after_clear_pushes_again() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.enqueue(&node));
        assert_eq!(inbox.drain().count(), 1);

        node.link.idle();
        assert!(inbox.enqueue(&node));
        assert_eq!(inbox.drain().count(), 1);
    }
}
