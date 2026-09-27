//! Multi-producer, single-consumer queue.
//!
//! Producers [`Mpsc::push`]. One consumer [`Mpsc::drain`]s the chain (newest
//! first). The CAS lives in [`super::stack`].

use core::{marker::PhantomData, ptr::NonNull};

use super::stack::{self, Linked};

/// Head of an intrusive MPSC queue. Nodes are borrowed for `'a`.
pub(crate) struct Mpsc<'a, T: Linked> {
    stack: stack::Stack<'a, T>,
}

impl<'a, T: Linked> Mpsc<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            stack: stack::Stack::new(),
        }
    }

    /// Link `node` in front of the head. `node` must not already be queued.
    #[inline]
    pub(crate) fn push(&self, node: &'a T) {
        self.stack.push(node);
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// Detach every node and walk them. Single consumer only. Newest first.
    pub(crate) fn drain(&self) -> Drain<'a, T> {
        Drain {
            next: self.stack.detach(),
            marker: PhantomData,
        }
    }
}

/// Nodes detached by [`Mpsc::drain`], newest first.
pub(crate) struct Drain<'a, T: Linked> {
    next: Option<NonNull<T>>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> Iterator for Drain<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        // SAFETY: every node was pushed as `&'a T`, and its `next` was stored before the
        // head CAS that published it. Callers re-push a node only after reading past it.
        let node = unsafe { self.next?.as_ref() };
        self.next = NonNull::new(node.links().load());
        Some(node)
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
        drained: AtomicBool,
    }

    impl TestNode {
        fn new() -> Self {
            Self {
                link: Link::new(),
                drained: AtomicBool::new(false),
            }
        }
    }

    impl Linked for TestNode {
        fn links(&self) -> &Link<Self> {
            &self.link
        }
    }

    fn addrs(drain: Drain<'_, TestNode>) -> Vec<usize> {
        drain.map(|node| ptr::from_ref(node).addr()).collect()
    }

    #[test]
    fn push_then_drain() {
        let queue = Mpsc::new();
        let node = TestNode::new();
        queue.push(&node);
        assert_eq!(addrs(queue.drain()), [ptr::from_ref(&node).addr()]);
        assert!(queue.is_empty());
    }

    #[test]
    fn drain_is_newest_first() {
        let queue = Mpsc::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        queue.push(&older);
        queue.push(&newer);
        assert_eq!(
            addrs(queue.drain()),
            [ptr::from_ref(&newer).addr(), ptr::from_ref(&older).addr()]
        );
    }

    #[test]
    fn drain_empty_yields_nothing() {
        let queue: Mpsc<'_, TestNode> = Mpsc::new();
        assert_eq!(queue.drain().count(), 0);
        assert!(queue.is_empty());
    }

    /// Drain racing a push must see either both nodes or leave the new one behind.
    #[test]
    fn push_vs_drain_keeps_older_node() {
        let queue = Mpsc::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        queue.push(&older);

        let drained = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| drained.store(queue.drain().count(), Ordering::Release));
            queue.push(&newer);
        });

        let remaining = queue.drain().count();
        assert_eq!(drained.load(Ordering::Acquire) + remaining, 2);
    }

    /// Producers racing a consumer: every node is drained exactly once.
    #[test]
    fn concurrent_push_drains_each_node_once() {
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 10_000;

        let queue = Mpsc::new();
        let pool: Vec<_> = (0..PRODUCERS * PER_PRODUCER)
            .map(|_| TestNode::new())
            .collect();
        let pushed = AtomicUsize::new(0);

        let mark = |node: &TestNode| {
            assert!(!node.drained.swap(true, Ordering::AcqRel), "drained twice");
        };

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
            while pushed.load(Ordering::Acquire) < pool.len() {
                queue.drain().for_each(mark);
                std::thread::yield_now();
            }
        });
        queue.drain().for_each(mark);

        assert!(pool.iter().all(|node| node.drained.load(Ordering::Acquire)));
        assert!(queue.is_empty());
    }
}
