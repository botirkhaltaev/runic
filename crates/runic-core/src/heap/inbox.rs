//! Owner inbox: an [`Mpsc`] queue that holds each run or extent at most once.
//!
//! [`Inbox::queue`] pushes a node only on its idle→queued transition, so many remote
//! frees against the same run collapse into one entry. The owner [`Inbox::drain`]s and
//! [`crate::heap::Run::accept`]s (or extent accept) claimed work in one pass.

use core::sync::atomic::{AtomicBool, Ordering};

use super::queue::{
    Drain, Mpsc,
    stack::{self, Linked},
};

/// Inbox membership embedded on `Run` / `Extent`: queue link plus a queued flag.
///
/// Idle (`queued == false`) means the entity is on no inbox and may be pushed again;
/// Queued means it is on exactly one inbox.
pub(crate) struct Link<T> {
    stack: stack::Link<T>,
    queued: AtomicBool,
}

impl<T> Link<T> {
    pub(crate) const fn new() -> Self {
        Self {
            stack: stack::Link::new(),
            queued: AtomicBool::new(false),
        }
    }

    /// Whether this entity is currently queued on an inbox.
    #[inline]
    pub(crate) fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }

    /// Idle → Queued. `true` when this call won the transition.
    ///
    /// Active freers take an enqueue lease before calling this for a new queue win so
    /// close cannot observe Queued without a subsequent [`Mpsc::push`]. Coalesced
    /// freers use [`Self::is_queued`] and skip the lease.
    #[inline]
    pub(crate) fn try_queue(&self) -> bool {
        !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Queued → Idle. Owner-only, before scanning claims on a just-dequeued node.
    #[inline]
    pub(crate) fn clear_queued(&self) {
        self.queued.store(false, Ordering::Release);
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

    /// Push `node` unless it is already queued. `true` when this call pushed it.
    pub(crate) fn queue(&self, node: &'a T) -> bool {
        if !node.link().try_queue() {
            return false;
        }
        self.queue.push(node);
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Take every queued node. The owner clears `queued` when it accepts each one.
    pub(crate) fn drain(&self) -> Drain<'a, T> {
        self.queue.drain()
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
    fn queue_pushes_once_until_cleared() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.queue(&node));
        assert!(!inbox.queue(&node));
        assert!(!inbox.queue(&node));
        assert_eq!(inbox.drain().count(), 1);
        assert!(inbox.is_empty());
    }

    #[test]
    fn queue_after_clear_pushes_again() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.queue(&node));
        assert_eq!(inbox.drain().count(), 1);

        node.link.clear_queued();
        assert!(inbox.queue(&node));
        assert_eq!(inbox.drain().count(), 1);
    }
}
