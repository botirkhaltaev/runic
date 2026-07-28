//! Intrusive multi-producer, single-consumer Treiber stack, coalesced by owner.
//!
//! [`Inbox`] carries at most one entry per run or extent at a time: [`Inbox::push`]
//! queues a node only on the idle→queued transition, so many remote frees against the
//! same run collapse into a single inbox entry. The owner [`Inbox::drain`]s and
//! [`crate::heap::Run::accept`]s (or extent accept) claimed work in one pass.
//!
//! Publication linearizes on a successful CAS of `head`: the node's next link to the
//! previous head is stored before that CAS, so a concurrent drain that observes the new
//! head always walks the full prior chain.

use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

/// Intrusive inbox membership: Treiber `next` link plus queued flag.
///
/// Embedded on the owning `Run` / `Extent`. Idle (`queued == false`) means the entity is
/// off every inbox and safe to re-link; Queued means it is linked into exactly one inbox.
pub(crate) struct InboxLink<T> {
    next: AtomicPtr<T>,
    queued: AtomicBool,
}

impl<T> InboxLink<T> {
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

/// Types that embed an [`InboxLink`] for coalesced inbox membership.
pub(crate) trait InboxNode: Sized {
    fn link(&self) -> &InboxLink<Self>;
}

/// Lock-free MPSC inbox of distinct owner entities (run or extent pointers).
///
/// Producers may only use shared references. Single-consumer `drain`.
pub(crate) struct Inbox<T: InboxNode> {
    /// Head of the pending intrusive chain (newer publishes link in front).
    head: AtomicPtr<T>,
}

// SAFETY: producers and the single consumer only coordinate through `head` and each node's
// intrusive `InboxLink::next`.
unsafe impl<T: InboxNode> Sync for Inbox<T> {}

impl<T: InboxNode> Inbox<T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Queue `node` if not already queued. Returns `true` when newly queued and linked.
    ///
    /// Already-queued → `false` (coalesce). Active freers that need an enqueue lease must
    /// use [`InboxLink::try_queue`] then [`Self::link`] under the lease instead, so coalesced
    /// claims skip the lease entirely.
    pub(crate) fn push(&self, node: NonNull<T>) -> bool {
        // SAFETY: `node` is a stable heap-owned entity for as long as it may be claimed.
        let link = unsafe { node.as_ref() }.link();
        if !link.try_queue() {
            return false;
        }
        self.link(node);
        true
    }

    /// Treiber-link an already-queued `node`. Caller won [`InboxLink::try_queue`] (or holds
    /// the heaps exclusive path for an exclusive drain-path link).
    pub(crate) fn link(&self, node: NonNull<T>) {
        // SAFETY: `node` is a stable heap-owned entity for as long as it may be claimed.
        let link = unsafe { node.as_ref() }.link();
        let raw = node.as_ptr();
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
    pub(crate) fn drain(&self) -> Option<InboxChain<T>> {
        let head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        NonNull::new(head).map(|first| InboxChain {
            cursor: Some(first),
        })
    }
}

/// Null-terminated intrusive chain detached by [`Inbox::drain`] (single walk for accept).
pub(crate) struct InboxChain<T: InboxNode> {
    cursor: Option<NonNull<T>>,
}

impl<T: InboxNode> Iterator for InboxChain<T> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.cursor?;
        // SAFETY: dequeued nodes keep their producer-linked next pointer valid until the
        // owner clears queued (`InboxLink::clear_queued`).
        let next = unsafe { node.as_ref() }.link().next.load(Ordering::Acquire);
        self.cursor = NonNull::new(next);
        Some(node)
    }
}

/// Coalesced inbox of remotely-freed runs.
pub(crate) type RunInbox = Inbox<crate::heap::Run>;
/// Coalesced inbox of remotely-freed extents.
pub(crate) type ExtentInbox = Inbox<crate::heap::Extent>;

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;

    #[repr(C)]
    struct TestNode {
        link: InboxLink<TestNode>,
        accepted: AtomicBool,
    }

    impl TestNode {
        fn new() -> Self {
            Self {
                link: InboxLink::new(),
                accepted: AtomicBool::new(false),
            }
        }
    }

    impl InboxNode for TestNode {
        fn link(&self) -> &InboxLink<Self> {
            &self.link
        }
    }

    fn node_ptr(node: &TestNode) -> NonNull<TestNode> {
        NonNull::from(node)
    }

    fn collect_chain(chain: InboxChain<TestNode>) -> Vec<NonNull<TestNode>> {
        chain.collect()
    }

    #[test]
    fn inbox_push_drain_single() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        assert!(inbox.push(ptr));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_repeated_push_before_drain_queues_once() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        assert!(inbox.push(ptr));
        assert!(!inbox.push(ptr));
        assert!(!inbox.push(ptr));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_push_after_clear_queued_requeues() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        assert!(inbox.push(ptr));
        assert!(inbox.drain().is_some());

        node.link.clear_queued();
        assert!(inbox.push(ptr));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
    }

    #[test]
    fn inbox_drain_lifo_across_pushes() {
        let inbox = Inbox::new();
        let first_node = TestNode::new();
        let second_node = TestNode::new();
        let first = node_ptr(&first_node);
        let second = node_ptr(&second_node);
        assert!(inbox.push(first));
        assert!(inbox.push(second));
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [second, first]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_drain_empty_is_none() {
        let inbox: Inbox<TestNode> = Inbox::new();
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
        let older_ptr = node_ptr(&older);
        let newer_ptr = node_ptr(&newer);

        assert!(inbox.push(older_ptr));

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
            assert!(inbox.push(newer_ptr));
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
                    assert!(inbox.push(node_ptr(node)));
                }
            });
            scope.spawn(|| {
                for node in &right {
                    assert!(inbox.push(node_ptr(node)));
                }
            });
        });

        let mut count = 0usize;
        while let Some(chain) = inbox.drain() {
            for ptr in chain {
                count += 1;
                let known = left.iter().chain(right.iter()).any(|n| node_ptr(n) == ptr);
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

        let inbox: Inbox<TestNode> = Inbox::new();
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
                        for ptr in chain {
                            // SAFETY: pointers drained here come from the fixed `pool` below.
                            let node = unsafe { ptr.as_ref() };
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
                let inbox = &inbox;
                let pool = &pool;
                let next_index = &next_index;
                scope.spawn(move || {
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
                        assert!(inbox.push(node_ptr(node)));
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
            for ptr in chain {
                // SAFETY: pointers drained here come from the fixed `pool` above.
                let node = unsafe { ptr.as_ref() };
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
