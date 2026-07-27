//! Intrusive multi-producer, single-consumer Treiber stack, coalesced by owner.
//!
//! Unlike a per-pointer remote-free queue, [`Inbox`] carries at most one entry per run or
//! extent at a time: a claim only pushes its owner once per idle→queued transition
//! ([`Notify::try_arm`]), so many remote frees against the same run collapse into a single
//! inbox entry instead of one entry per freed block. The owner drains the run/extent
//! pointer and scans/accepts everything claimed on it in one pass (see `Run::accept_remote`).
//!
//! Publication linearizes on a successful CAS of `head`: the node's next link to the
//! previous head is stored before that CAS, so a concurrent [`Inbox::drain`] that
//! observes the new head always walks the full prior chain.

use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

/// Idle/Queued arm state plus the intrusive Treiber-stack link for one entity.
///
/// Embedded by value on the owning `Run` / `Extent`. Idle means the entity is off every
/// inbox and safe to re-link; Queued means it is linked into exactly one inbox (in transit
/// or awaiting drain) and `next` must not be touched by anything but that inbox.
pub(crate) struct Notify<T> {
    next: AtomicPtr<T>,
    queued: AtomicBool,
}

impl<T> Notify<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(false),
        }
    }

    /// Idle → Queued. Returns `true` when this call won the transition and must publish
    /// the owning entity on its inbox (or, from `accept_remote`, republish it directly).
    #[inline]
    pub(crate) fn try_arm(&self) -> bool {
        !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Queued → Idle. Only the single consumer holding a just-dequeued node may call this;
    /// it must run before that node's claims are scanned (see `Run::accept_remote`).
    #[inline]
    pub(crate) fn disarm(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

/// Entities with an intrusive [`Notify`] link, coalesced through one [`Inbox`].
pub(crate) trait Notified: Sized {
    fn notify(&self) -> &Notify<Self>;
}

/// Lock-free MPSC inbox of distinct notified entities (run or extent pointers).
///
/// Producers may only use shared references. Single-consumer `drain`.
pub(crate) struct Inbox<T: Notified> {
    /// Head of the pending intrusive chain (newer publishes link in front).
    head: AtomicPtr<T>,
}

// SAFETY: producers and the single consumer only coordinate through `head` and each node's
// intrusive `Notify::next`.
unsafe impl<T: Notified> Sync for Inbox<T> {}

impl<T: Notified> Inbox<T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Push `node`, assuming the caller already won the idle→queued transition
    /// (`Notify::try_arm`, or `accept_remote`'s own re-arm after a straggling claim).
    /// Does not check or touch `Notify::queued` — a second push of an already-Queued
    /// node would corrupt the chain, so every caller must own a fresh `try_arm` win.
    pub(crate) fn republish(&self, node: NonNull<T>) {
        let raw = node.as_ptr();
        // SAFETY: `node` is a stable heap-owned entity for as long as it may be claimed.
        let next = &unsafe { node.as_ref() }.notify().next;
        let mut old = self.head.load(Ordering::Acquire);
        loop {
            // Store the tail link before publishing the new head so a concurrent drain
            // that observes `raw` always continues into the prior chain.
            next.store(old, Ordering::Release);
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
pub(crate) struct InboxChain<T: Notified> {
    cursor: Option<NonNull<T>>,
}

impl<T: Notified> Iterator for InboxChain<T> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.cursor?;
        // SAFETY: dequeued nodes keep their producer-linked next pointer valid until the
        // owner disarms them (`Notify::disarm`).
        let next = unsafe { node.as_ref() }
            .notify()
            .next
            .load(Ordering::Acquire);
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
        notify: Notify<TestNode>,
        accepted: AtomicBool,
    }

    impl TestNode {
        fn new() -> Self {
            Self {
                notify: Notify::new(),
                accepted: AtomicBool::new(false),
            }
        }
    }

    impl Notified for TestNode {
        fn notify(&self) -> &Notify<Self> {
            &self.notify
        }
    }

    fn node_ptr(node: &TestNode) -> NonNull<TestNode> {
        NonNull::from(node)
    }

    fn collect_chain(chain: InboxChain<TestNode>) -> Vec<NonNull<TestNode>> {
        chain.collect()
    }

    /// Test helper mirroring production: `try_arm` then `republish`.
    fn push_node(inbox: &Inbox<TestNode>, ptr: NonNull<TestNode>) {
        // SAFETY: test nodes live for the whole test.
        if unsafe { ptr.as_ref() }.notify().try_arm() {
            inbox.republish(ptr);
        }
    }

    #[test]
    fn inbox_notify_drain_single() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        push_node(&inbox, ptr);
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_repeated_notify_before_drain_pushes_once() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        push_node(&inbox, ptr);
        push_node(&inbox, ptr);
        push_node(&inbox, ptr);
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_notify_after_disarm_requeues() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        push_node(&inbox, ptr);
        assert!(inbox.drain().is_some());

        node.notify.disarm();
        push_node(&inbox, ptr);
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
    }

    #[test]
    fn inbox_notify_drain_lifo_across_pushes() {
        let inbox = Inbox::new();
        let first_node = TestNode::new();
        let second_node = TestNode::new();
        let first = node_ptr(&first_node);
        let second = node_ptr(&second_node);
        push_node(&inbox, first);
        push_node(&inbox, second);
        // Newer publish is drained first.
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

    #[test]
    fn republish_bypasses_arm_check() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        // Simulate accept_remote's own re-arm winning before asking the inbox to publish.
        assert!(node.notify.try_arm());
        inbox.republish(ptr);
        let chain = inbox.drain().unwrap();
        assert_eq!(collect_chain(chain), [ptr]);
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

        push_node(&inbox, older_ptr);

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
            push_node(&inbox, newer_ptr);
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
                    push_node(&inbox, node_ptr(node));
                }
            });
            scope.spawn(|| {
                for node in &right {
                    push_node(&inbox, node_ptr(node));
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

    /// 10_000-iteration multi-producer / drain stress: no lost nodes, and producers
    /// never observe a node accepted twice.
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
                        push_node(inbox, node_ptr(node));
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
