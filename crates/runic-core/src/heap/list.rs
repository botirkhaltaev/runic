//! Intrusive lock-free singly linked list (the Linux `llist` shape).
//!
//! Any thread may `push` onto the head. One consumer `drain`s the whole list with a
//! single swap and walks it. Order is newest first. Coalesce-by-owner lives on
//! [`super::inbox::Inbox`].

use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

/// Types that embed the intrusive `next` pointer for [`List`].
pub(crate) trait Linked: Sized {
    fn next(&self) -> &AtomicPtr<Self>;
}

/// Head of an intrusive list of nodes borrowed for `'a`.
pub(crate) struct List<'a, T: Linked> {
    head: AtomicPtr<T>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> List<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            marker: PhantomData,
        }
    }

    /// Link `node` in front of the head. `node` must not already be on a list.
    pub(crate) fn push(&self, node: &'a T) {
        let raw = ptr::from_ref(node).cast_mut();
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // Store `next` before publishing `node` so a drain that sees it walks the rest.
            node.next().store(head, Ordering::Release);
            match self
                .head
                .compare_exchange_weak(head, raw, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Detach every node and walk them. Single consumer only.
    pub(crate) fn drain(&self) -> Drain<'a, T> {
        Drain {
            next: NonNull::new(self.head.swap(ptr::null_mut(), Ordering::AcqRel)),
            marker: PhantomData,
        }
    }
}

/// Nodes detached by [`List::drain`], newest first.
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
        self.next = NonNull::new(node.next().load(Ordering::Acquire));
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    struct TestNode {
        next: AtomicPtr<TestNode>,
        drained: AtomicBool,
    }

    impl TestNode {
        fn new() -> Self {
            Self {
                next: AtomicPtr::new(ptr::null_mut()),
                drained: AtomicBool::new(false),
            }
        }
    }

    impl Linked for TestNode {
        fn next(&self) -> &AtomicPtr<Self> {
            &self.next
        }
    }

    fn addrs(drain: Drain<'_, TestNode>) -> Vec<usize> {
        drain.map(|node| ptr::from_ref(node).addr()).collect()
    }

    #[test]
    fn push_then_drain() {
        let list = List::new();
        let node = TestNode::new();
        list.push(&node);
        assert_eq!(addrs(list.drain()), [ptr::from_ref(&node).addr()]);
        assert!(list.is_empty());
    }

    #[test]
    fn drain_is_newest_first() {
        let list = List::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        list.push(&older);
        list.push(&newer);
        assert_eq!(
            addrs(list.drain()),
            [ptr::from_ref(&newer).addr(), ptr::from_ref(&older).addr()]
        );
    }

    #[test]
    fn drain_empty_yields_nothing() {
        let list: List<'_, TestNode> = List::new();
        assert_eq!(list.drain().count(), 0);
        assert!(list.is_empty());
    }

    /// Drain racing a push must see either both nodes or leave the new one behind.
    #[test]
    fn push_vs_drain_keeps_older_node() {
        let list = List::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        list.push(&older);

        let drained = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| drained.store(list.drain().count(), Ordering::Release));
            list.push(&newer);
        });

        let remaining = list.drain().count();
        assert_eq!(drained.load(Ordering::Acquire) + remaining, 2);
    }

    /// Producers racing a consumer: every node is drained exactly once.
    #[test]
    fn concurrent_push_drains_each_node_once() {
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 10_000;

        let list = List::new();
        let pool: Vec<_> = (0..PRODUCERS * PER_PRODUCER)
            .map(|_| TestNode::new())
            .collect();
        let pushed = AtomicUsize::new(0);

        let mark = |node: &TestNode| {
            assert!(!node.drained.swap(true, Ordering::AcqRel), "drained twice");
        };

        std::thread::scope(|scope| {
            for chunk in pool.chunks(PER_PRODUCER) {
                let (list, pushed) = (&list, &pushed);
                scope.spawn(move || {
                    for node in chunk {
                        list.push(node);
                        pushed.fetch_add(1, Ordering::Release);
                    }
                });
            }
            while pushed.load(Ordering::Acquire) < pool.len() {
                list.drain().for_each(mark);
                std::thread::yield_now();
            }
        });
        list.drain().for_each(mark);

        assert!(pool.iter().all(|node| node.drained.load(Ordering::Acquire)));
        assert!(list.is_empty());
    }
}
