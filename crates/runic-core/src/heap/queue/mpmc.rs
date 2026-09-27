//! Multi-producer, multi-consumer queue.
//!
//! [`Mpmc::push`] and [`Mpmc::pop`] are both concurrent. The CAS is the same
//! stack as [`super::Mpsc`]; this type does not offer `drain`, because any
//! thread may pop.

use super::stack::{self, Linked};

/// Head of an intrusive MPMC stack. Nodes are borrowed for `'a`.
pub(crate) struct Mpmc<'a, T: Linked> {
    stack: stack::Stack<'a, T>,
}

impl<'a, T: Linked> Mpmc<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            stack: stack::Stack::new(),
        }
    }

    /// Link `node` in front of the head. `node` must not already be on a stack.
    #[inline]
    pub(crate) fn push(&self, node: &'a T) {
        self.stack.push(node);
    }

    /// Pop the newest node. Concurrent with `push` and other `pop`s.
    #[inline]
    pub(crate) fn pop(&self) -> Option<&'a T> {
        self.stack.pop()
    }
}

#[cfg(test)]
mod tests {
    use core::{
        ptr,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::super::stack::{Link, Linked};
    use super::*;

    struct TestNode {
        link: Link<TestNode>,
        popped: AtomicBool,
    }

    impl TestNode {
        fn new() -> Self {
            Self {
                link: Link::new(),
                popped: AtomicBool::new(false),
            }
        }
    }

    impl Linked for TestNode {
        fn links(&self) -> &Link<Self> {
            &self.link
        }
    }

    #[test]
    fn push_then_pop_is_lifo() {
        let queue = Mpmc::new();
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
    }

    /// Producers and consumers: every node is popped exactly once.
    #[test]
    fn concurrent_push_pops_each_node_once() {
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 2_000;

        let pool: Vec<_> = (0..PRODUCERS * PER_PRODUCER)
            .map(|_| TestNode::new())
            .collect();
        let queue = Mpmc::new();
        let pushed = AtomicUsize::new(0);
        let popped = AtomicUsize::new(0);
        let total = pool.len();

        std::thread::scope(|scope| {
            for chunk in pool.chunks(PER_PRODUCER) {
                let (queue, pushed) = (&queue, &pushed);
                scope.spawn(move || {
                    for node in chunk {
                        queue.push(node);
                        pushed.fetch_add(1, Ordering::Release);
                    }
                });
            }
            for _ in 0..PRODUCERS {
                let (queue, popped, pushed) = (&queue, &popped, &pushed);
                scope.spawn(move || {
                    while popped.load(Ordering::Acquire) < total {
                        if let Some(node) = queue.pop() {
                            assert!(!node.popped.swap(true, Ordering::AcqRel), "popped twice");
                            popped.fetch_add(1, Ordering::Release);
                        } else if pushed.load(Ordering::Acquire) == total {
                            break;
                        } else {
                            std::thread::yield_now();
                        }
                    }
                });
            }
        });

        while let Some(node) = queue.pop() {
            assert!(!node.popped.swap(true, Ordering::AcqRel), "popped twice");
            popped.fetch_add(1, Ordering::Release);
        }

        assert_eq!(popped.load(Ordering::Acquire), total);
        assert!(pool.iter().all(|node| node.popped.load(Ordering::Acquire)));
        assert!(queue.pop().is_none());
    }
}
