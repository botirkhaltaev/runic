//! Intrusive multi-producer, single-consumer Treiber stack, coalesced by owner.
//!
//! [`Inbox`] carries at most one entry per run or extent at a time: [`Inbox::queue`]
//! queues a node only on the idle→queued transition, so many remote frees against the
//! same run collapse into a single inbox entry. The owner [`Inbox::drain`]s and
//! [`crate::heap::Run::accept`]s (or extent accept) claimed work in one pass.
//!
//! Publication linearizes on a successful CAS of `head`: the node's next link to the
//! previous head is stored before that CAS, so a concurrent drain that observes the new
//! head always walks the full prior chain.

use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

/// Intrusive inbox membership: Treiber `next` link plus queued flag.
///
/// Embedded on the owning `Run` / `Extent`. Idle (`queued == false`) means the entity is
/// off every inbox and safe to re-link; Queued means it is linked into exactly one inbox.
pub(crate) struct Link<T> {
    next: AtomicPtr<T>,
    queued: AtomicBool,
}

impl<T> Link<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
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
    /// close cannot observe Queued without a subsequent [`Inbox::link`]. Coalesced
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

/// Lock-free MPSC inbox of distinct owner entities (run or extent).
///
/// Producers may only queue nodes borrowed for `'a`; every drained node retains
/// that same lifetime. Single-consumer `drain`.
pub(crate) struct Inbox<'a, T: Node> {
    /// Head of the pending intrusive chain (newer publishes link in front).
    head: AtomicPtr<T>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Node> Inbox<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            marker: PhantomData,
        }
    }

    /// Queue `node` if not already queued. Returns `true` when newly queued and linked.
    pub(crate) fn queue(&self, node: &'a T) -> bool {
        let link = node.link();
        if !link.try_queue() {
            return false;
        }
        self.link(node);
        true
    }

    /// Treiber-link an already-queued `node`. Caller won [`Link::try_queue`] (or holds
    /// the heaps exclusive path for an exclusive drain-path link).
    fn link(&self, node: &'a T) {
        let link = node.link();
        let raw = core::ptr::from_ref(node).cast_mut();
        let mut old = self.head.load(Ordering::Acquire);
        loop {
            // Store the tail link before publishing the new head so a concurrent drain
            // that observes `raw` always continues into the prior chain.
            link.next.store(old, Ordering::Release);
            match self
                .head
                .compare_exchange_weak(old, raw, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(current) => old = current,
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Detach the entire pending chain. Single-consumer only.
    ///
    /// Returns a null-terminated walk (one pass). Empty → `None`.
    pub(crate) fn drain(&self) -> Option<Chain<'a, T>> {
        let head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        NonNull::new(head).map(|first| Chain {
            cursor: Some(first),
            marker: PhantomData,
        })
    }
}

/// Null-terminated intrusive chain detached by [`Inbox::drain`] (single walk for accept).
///
/// The borrow is tied to the queued nodes, not to the call-scoped inbox borrow.
/// Production inboxes use `'static` run headers and extent slots.
pub(crate) struct Chain<'a, T: Node> {
    cursor: Option<NonNull<T>>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Node> Chain<'a, T> {
    fn step(node: NonNull<T>) -> (&'a T, Option<NonNull<T>>) {
        // SAFETY: dequeued nodes keep their producer-linked next pointer valid until the
        // owner clears queued (`Link::clear_queued`).
        let node = unsafe { node.as_ref() };
        let next = NonNull::new(node.link().next.load(Ordering::Acquire));
        (node, next)
    }
}

impl<'a, T: Node> Iterator for Chain<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.cursor?;
        let (item, next) = Self::step(node);
        self.cursor = next;
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;

    #[repr(C)]
    struct TestNode {
        link: Link<TestNode>,
        accepted: AtomicBool,
    }

    impl PartialEq for TestNode {
        fn eq(&self, other: &Self) -> bool {
            core::ptr::eq(self, other)
        }
    }

    impl Eq for TestNode {}

    impl TestNode {
        fn new() -> Self {
            Self {
                link: Link::new(),
                accepted: AtomicBool::new(false),
            }
        }
    }

    impl Node for TestNode {
        fn link(&self) -> &Link<Self> {
            &self.link
        }
    }

    fn collect_chain(chain: Chain<'_, TestNode>) -> Vec<usize> {
        chain.map(|node| core::ptr::from_ref(node).addr()).collect()
    }

    #[test]
    fn inbox_push_drain_single() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.queue(&node));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [core::ptr::from_ref(&node).addr()]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_repeated_push_before_drain_queues_once() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.queue(&node));
        assert!(!inbox.queue(&node));
        assert!(!inbox.queue(&node));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [core::ptr::from_ref(&node).addr()]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_push_after_clear_queued_requeues() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        assert!(inbox.queue(&node));
        assert!(inbox.drain().is_some());

        node.link.clear_queued();
        assert!(inbox.queue(&node));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [core::ptr::from_ref(&node).addr()]);
    }

    #[test]
    fn inbox_drain_lifo_across_pushes() {
        let inbox = Inbox::new();
        let first_node = TestNode::new();
        let second_node = TestNode::new();
        assert!(inbox.queue(&first_node));
        assert!(inbox.queue(&second_node));
        let chain = inbox.drain().unwrap();
        assert_eq!(
            collect_chain(chain),
            [
                core::ptr::from_ref(&second_node).addr(),
                core::ptr::from_ref(&first_node).addr(),
            ]
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_drain_empty_is_none() {
        let inbox: Inbox<'_, TestNode> = Inbox::new();
        assert!(inbox.drain().is_none());
        assert!(inbox.is_empty());
    }

    /// Deterministic interleaving: drain observes the new head only after the
    /// producer has linked the previous head through the node's next pointer.
    #[test]
    fn push_vs_drain_preserves_prior_chain() {
        let inbox = Inbox::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        assert!(inbox.queue(&older));

        let published = AtomicBool::new(false);
        let drained = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !published.load(AtomicOrdering::Acquire) {
                    core::hint::spin_loop();
                }
                std::thread::yield_now();
                if let Some(chain) = inbox.drain() {
                    drained.store(chain.count(), AtomicOrdering::Release);
                }
            });

            published.store(true, AtomicOrdering::Release);
            assert!(inbox.queue(&newer));
        });

        let seen = drained.load(AtomicOrdering::Acquire);
        let remaining = inbox.drain().map_or(0, Iterator::count);
        assert_eq!(
            seen + remaining,
            2,
            "push-vs-drain must preserve both nodes (drained={seen}, remaining={remaining})"
        );
    }

    /// Two producers racing distinct nodes; every node must appear exactly once across drains.
    #[test]
    fn two_producers_preserve_all_nodes() {
        const PER_PRODUCER: usize = 256;
        let inbox = Inbox::new();
        let left: Vec<_> = (0..PER_PRODUCER).map(|_| TestNode::new()).collect();
        let right: Vec<_> = (0..PER_PRODUCER).map(|_| TestNode::new()).collect();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for node in &left {
                    assert!(inbox.queue(node));
                }
            });
            scope.spawn(|| {
                for node in &right {
                    assert!(inbox.queue(node));
                }
            });
        });

        let mut count = 0usize;
        while let Some(chain) = inbox.drain() {
            for node in chain {
                count += 1;
                let known = left.iter().chain(right.iter()).any(|n| n == node);
                assert!(known, "unknown pointer drained");
            }
        }
        assert_eq!(count, PER_PRODUCER * 2);
    }

    /// Multi-producer / drain stress: no lost nodes, and producers never observe a node
    /// accepted twice.
    #[test]
    fn multi_producer_drain_stress_no_lost_nodes() {
        const ITERATIONS: usize = 10_000;
        const PRODUCERS: usize = 4;
        const PER_ITER: usize = PRODUCERS;

        let inbox: Inbox<'_, TestNode> = Inbox::new();
        let pool: Vec<_> = (0..ITERATIONS * PER_ITER)
            .map(|_| TestNode::new())
            .collect();
        let next_index = AtomicUsize::new(0);
        let accepted_total = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let consumer = scope.spawn(|| {
                let mut local = 0usize;
                while !stop.load(AtomicOrdering::Acquire) || !inbox.is_empty() {
                    if let Some(chain) = inbox.drain() {
                        for node in chain {
                            assert!(
                                !node.accepted.swap(true, AtomicOrdering::AcqRel),
                                "double accept"
                            );
                            local += 1;
                        }
                    } else {
                        std::thread::yield_now();
                    }
                }
                accepted_total.store(local, AtomicOrdering::Release);
            });

            for _ in 0..PRODUCERS {
                scope.spawn(|| {
                    loop {
                        let i = next_index.fetch_add(1, AtomicOrdering::Relaxed);
                        if i >= ITERATIONS * PER_ITER {
                            break;
                        }
                        let node = &pool[i];
                        assert!(
                            !node.accepted.load(AtomicOrdering::Acquire),
                            "producer must not publish an already-accepted node"
                        );
                        assert!(inbox.queue(node));
                    }
                });
            }

            while next_index.load(AtomicOrdering::Acquire) < ITERATIONS * PER_ITER {
                std::thread::yield_now();
            }
            std::thread::yield_now();
            stop.store(true, AtomicOrdering::Release);
            let _ = consumer.join();
        });

        while let Some(chain) = inbox.drain() {
            for node in chain {
                assert!(
                    !node.accepted.swap(true, AtomicOrdering::AcqRel),
                    "node accepted twice on final sweep"
                );
            }
        }

        let accepted = pool
            .iter()
            .filter(|n| n.accepted.load(AtomicOrdering::Acquire))
            .count();
        assert_eq!(accepted, ITERATIONS * PER_ITER, "lost or duplicate nodes");
        assert!(accepted_total.load(AtomicOrdering::Acquire) <= accepted);
        assert!(inbox.is_empty());
    }
}
