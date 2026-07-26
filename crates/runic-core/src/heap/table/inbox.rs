//! Intrusive multi-producer, single-consumer queue for remote frees.
//!
//! Treiber-style stack of intrusive chains (`RemoteList` batches). Empty state is
//! a null head pointer — no self-reference, so [`Inbox`] is movable after [`Inbox::new`].
//!
//! Publication linearizes on a successful CAS of `head`: the batch tail link to the
//! previous head is stored before that CAS, so a concurrent [`Inbox::drain`] that
//! observes the new head always walks the full prior chain.

use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

/// Intrusive chain of remote-pending user blocks (FIFO within a publish).
///
/// `first` and `last` are always valid, non-null ends: a `RemoteList` is only ever
/// constructed from a batch that already contains at least one pointer.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RemoteList {
    pub(crate) first: NonNull<u8>,
    pub(crate) last: NonNull<u8>,
    cursor: Option<NonNull<u8>>,
}

impl RemoteList {
    pub(crate) fn from_ends(first: NonNull<u8>, last: NonNull<u8>) -> Self {
        Self {
            first,
            last,
            cursor: Some(first),
        }
    }
}

impl Iterator for RemoteList {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let ptr = self.cursor?;
        if ptr == self.last {
            self.cursor = None;
        } else {
            // SAFETY: nodes between first and last were linked by the producer.
            let next = unsafe { &*ptr.as_ptr().cast::<AtomicPtr<u8>>() }.load(Ordering::Acquire);
            self.cursor = NonNull::new(next);
        }
        Some(ptr)
    }
}

/// Null-terminated intrusive chain detached by [`Inbox::drain`] (single walk for accept).
pub(crate) struct RemoteChain {
    cursor: Option<NonNull<u8>>,
}

impl Iterator for RemoteChain {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let ptr = self.cursor?;
        // SAFETY: drained nodes keep producer-linked next words until the owner accepts them.
        let next = unsafe { &*ptr.as_ptr().cast::<AtomicPtr<u8>>() }.load(Ordering::Acquire);
        self.cursor = NonNull::new(next);
        Some(ptr)
    }
}

/// Lock-free MPSC inbox. Producers may only use shared references.
pub(crate) struct Inbox {
    /// Head of the pending intrusive chain (newer publishes link in front).
    head: AtomicPtr<u8>,
}

// SAFETY: producers and the single consumer only coordinate through `head` and
// in-block next words.
unsafe impl Sync for Inbox {}

impl Inbox {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn next_of(node: *mut u8) -> &'static AtomicPtr<u8> {
        // SAFETY: remote-pending user blocks store the intrusive next at the base address.
        // The reference is only used while the block remains remote-pending.
        unsafe { &*node.cast::<AtomicPtr<u8>>() }
    }

    /// Publish `list` in front of the current head.
    ///
    /// Links `list.last → old_head` before a CAS of `head: old_head → list.first`.
    /// On CAS failure the tail link is rewritten and the attempt retries.
    pub(crate) fn push_batch(&self, list: &RemoteList) {
        let first = list.first.as_ptr();
        let last_next = Self::next_of(list.last.as_ptr());
        let mut old = self.head.load(Ordering::Acquire);
        loop {
            // Store the tail link before publishing the new head so a concurrent
            // drain that observes `first` always continues into `old`.
            last_next.store(old, Ordering::Release);
            match self
                .head
                .compare_exchange_weak(old, first, Ordering::AcqRel, Ordering::Acquire)
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
    /// Returns a null-terminated walk (one pass). Across publishes the order is LIFO;
    /// within a batch it stays FIFO. Empty → `None`.
    pub(crate) fn drain(&self) -> Option<RemoteChain> {
        let first_ptr = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        let first = NonNull::new(first_ptr)?;
        Some(RemoteChain {
            cursor: Some(first),
        })
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[repr(C)]
    struct TestNode {
        next: AtomicPtr<u8>,
        /// Set when the single consumer accepts the node; producers must not
        /// write `next` after this becomes true.
        accepted: AtomicBool,
    }

    impl TestNode {
        const fn new() -> Self {
            Self {
                next: AtomicPtr::new(ptr::null_mut()),
                accepted: AtomicBool::new(false),
            }
        }
    }

    fn node_ptr(node: &TestNode) -> NonNull<u8> {
        NonNull::new(core::ptr::from_ref(node).cast::<u8>().cast_mut()).unwrap()
    }

    fn collect_list(list: impl Iterator<Item = NonNull<u8>>) -> [Option<NonNull<u8>>; 4] {
        let mut out = [None; 4];
        for (i, ptr) in list.enumerate() {
            out[i] = Some(ptr);
        }
        out
    }

    fn accept_all(list: impl Iterator<Item = NonNull<u8>>, pool: &[TestNode]) {
        for ptr in list {
            let node = pool
                .iter()
                .find(|node| node_ptr(node) == ptr)
                .expect("drained pointer not in pool");
            assert!(
                !node.accepted.swap(true, Ordering::AcqRel),
                "node accepted twice"
            );
        }
    }

    #[test]
    fn inbox_push_drain_single() {
        let inbox = Inbox::new();
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
        let list = inbox.drain().unwrap();
        assert_eq!(collect_list(list), [Some(ptr), None, None, None]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_push_drain_lifo_across_batches() {
        let inbox = Inbox::new();
        let first_node = TestNode::new();
        let second_node = TestNode::new();
        let first = node_ptr(&first_node);
        let second = node_ptr(&second_node);
        inbox.push_batch(&RemoteList::from_ends(first, first));
        inbox.push_batch(&RemoteList::from_ends(second, second));
        // Newer publish is drained first.
        let list = inbox.drain().unwrap();
        assert_eq!(collect_list(list), [Some(second), Some(first), None, None]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_push_batch_preserves_chain_order() {
        let inbox = Inbox::new();
        let first_node = TestNode::new();
        let second_node = TestNode::new();
        let first = node_ptr(&first_node);
        let second = node_ptr(&second_node);
        first_node.next.store(second.as_ptr(), Ordering::Relaxed);
        inbox.push_batch(&RemoteList::from_ends(first, second));
        let list = inbox.drain().unwrap();
        assert_eq!(collect_list(list), [Some(first), Some(second), None, None]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_is_movable_after_new() {
        let inbox = Inbox::new();
        let moved = inbox;
        let node = TestNode::new();
        let ptr = node_ptr(&node);
        moved.push_batch(&RemoteList::from_ends(ptr, ptr));
        let list = moved.drain().unwrap();
        assert_eq!(collect_list(list), [Some(ptr), None, None, None]);
        assert!(moved.is_empty());
    }

    /// Deterministic interleaving: drain observes the new head only after the
    /// producer has linked the previous head through the batch tail.
    #[test]
    fn push_vs_drain_preserves_prior_chain() {
        let inbox = Inbox::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        let older_ptr = node_ptr(&older);
        let newer_ptr = node_ptr(&newer);

        inbox.push_batch(&RemoteList::from_ends(older_ptr, older_ptr));

        let published = AtomicBool::new(false);
        let drained = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !published.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                // Yield once so the producer's CAS and drain race more often.
                std::thread::yield_now();
                if let Some(list) = inbox.drain() {
                    drained.store(list.count(), Ordering::Release);
                }
            });

            published.store(true, Ordering::Release);
            inbox.push_batch(&RemoteList::from_ends(newer_ptr, newer_ptr));
        });

        let seen = drained.load(Ordering::Acquire);
        let remaining = inbox.drain().map_or(0, Iterator::count);
        assert_eq!(
            seen + remaining,
            2,
            "push-vs-drain must preserve both nodes (drained={seen}, remaining={remaining})"
        );
    }

    /// Two producers racing publish; every node must appear exactly once across drains.
    #[test]
    fn two_producers_preserve_all_nodes() {
        const PER_PRODUCER: usize = 256;
        let inbox = Inbox::new();
        let left: Vec<_> = (0..PER_PRODUCER).map(|_| TestNode::new()).collect();
        let right: Vec<_> = (0..PER_PRODUCER).map(|_| TestNode::new()).collect();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for node in &left {
                    let ptr = node_ptr(node);
                    inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
                }
            });
            scope.spawn(|| {
                for node in &right {
                    let ptr = node_ptr(node);
                    inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
                }
            });
        });

        let mut count = 0usize;
        while let Some(list) = inbox.drain() {
            for ptr in list {
                count += 1;
                let known = left.iter().chain(right.iter()).any(|n| node_ptr(n) == ptr);
                assert!(known, "unknown pointer drained");
            }
        }
        assert_eq!(count, PER_PRODUCER * 2);
    }

    /// CAS-retry path: concurrent publishers force head rewrites; chain still intact.
    #[test]
    fn cas_retry_preserves_multi_node_batches() {
        const BATCHES: usize = 64;
        let inbox = Inbox::new();
        let nodes: Vec<_> = (0..BATCHES * 2).map(|_| TestNode::new()).collect();

        std::thread::scope(|scope| {
            for chunk in nodes.chunks(2) {
                let inbox = &inbox;
                scope.spawn(move || {
                    let first = node_ptr(&chunk[0]);
                    let second = node_ptr(&chunk[1]);
                    chunk[0].next.store(second.as_ptr(), Ordering::Relaxed);
                    inbox.push_batch(&RemoteList::from_ends(first, second));
                });
            }
        });

        let mut count = 0usize;
        while let Some(list) = inbox.drain() {
            let collected: Vec<_> = list.collect();
            assert!(
                collected.len() % 2 == 0,
                "intra-batch FIFO chain must stay intact"
            );
            for window in collected.chunks(2) {
                // Each producer published a 2-node FIFO batch; LIFO across batches
                // may interleave batches but each pair remains consecutive.
                let a = window[0];
                let b = window[1];
                let pair = nodes
                    .chunks(2)
                    .any(|chunk| node_ptr(&chunk[0]) == a && node_ptr(&chunk[1]) == b);
                assert!(pair, "drained pair is not a published batch");
            }
            count += collected.len();
        }
        assert_eq!(count, BATCHES * 2);
    }

    /// 10_000-iteration multi-producer / drain stress: no lost nodes, and producers
    /// never write `next` after the consumer accepts a node.
    #[test]
    fn multi_producer_drain_stress_no_lost_nodes() {
        const ITERATIONS: usize = 10_000;
        const PRODUCERS: usize = 4;
        const PER_ITER: usize = PRODUCERS;

        let inbox = Inbox::new();
        // Reuse a fixed pool: each iteration publishes fresh unaccepted nodes.
        let pool: Vec<_> = (0..ITERATIONS * PER_ITER)
            .map(|_| TestNode::new())
            .collect();
        let next_index = AtomicUsize::new(0);
        let accepted_total = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let consumer = scope.spawn(|| {
                let mut local = 0usize;
                while !stop.load(Ordering::Acquire) || !inbox.is_empty() {
                    if let Some(list) = inbox.drain() {
                        for ptr in list {
                            for node in &pool {
                                if node_ptr(node) == ptr {
                                    assert!(
                                        !node.accepted.swap(true, Ordering::AcqRel),
                                        "double accept"
                                    );
                                    // Poison next after accept: a late producer write
                                    // would be visible as a non-null/non-sentinel value
                                    // if we ever re-linked an accepted node.
                                    node.next.store(ptr::null_mut(), Ordering::Release);
                                    local += 1;
                                    break;
                                }
                            }
                        }
                    } else {
                        std::thread::yield_now();
                    }
                }
                accepted_total.store(local, Ordering::Release);
            });

            for producer in 0..PRODUCERS {
                let inbox = &inbox;
                let pool = &pool;
                let next_index = &next_index;
                scope.spawn(move || {
                    let _ = producer;
                    loop {
                        let i = next_index.fetch_add(1, Ordering::Relaxed);
                        if i >= ITERATIONS * PER_ITER {
                            break;
                        }
                        let node = &pool[i];
                        assert!(
                            !node.accepted.load(Ordering::Acquire),
                            "producer must not publish an already-accepted node"
                        );
                        let ptr = node_ptr(node);
                        // Clear next before publish; only push_batch may write it
                        // while the node is remote-pending.
                        node.next.store(ptr::null_mut(), Ordering::Relaxed);
                        inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
                    }
                });
            }

            // Wait for all publish slots to be claimed, then stop the consumer.
            while next_index.load(Ordering::Acquire) < ITERATIONS * PER_ITER {
                std::thread::yield_now();
            }
            // Allow in-flight pushes to finish.
            std::thread::yield_now();
            stop.store(true, Ordering::Release);
            let _ = consumer.join();
        });

        // Sweep any straggler the consumer might have raced past on stop.
        while let Some(list) = inbox.drain() {
            accept_all(list, &pool);
        }

        let accepted = pool
            .iter()
            .filter(|n| n.accepted.load(Ordering::Acquire))
            .count();
        assert_eq!(accepted, ITERATIONS * PER_ITER, "lost or duplicate nodes");
        assert!(accepted_total.load(Ordering::Acquire) <= accepted);
        assert!(inbox.is_empty());
    }
}
