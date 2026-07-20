//! Intrusive multi-producer, single-consumer queue for remote frees.
//!
//! Treiber-style stack of intrusive chains (`RemoteList` batches). Empty state is
//! a null head pointer — no self-reference, so [`Inbox`] is movable after [`Inbox::new`].

use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

/// Intrusive chain of remote-pending user blocks (FIFO within a publish).
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RemoteList {
    pub(crate) first: Option<NonNull<u8>>,
    pub(crate) last: NonNull<u8>,
}

impl RemoteList {
    pub(crate) fn from_ends(first: NonNull<u8>, last: NonNull<u8>) -> Self {
        Self {
            first: Some(first),
            last,
        }
    }
}

impl Iterator for RemoteList {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let ptr = self.first?;
        if ptr == self.last {
            self.first = None;
        } else {
            // SAFETY: nodes between first and last were linked by the producer.
            let next = unsafe { &*ptr.as_ptr().cast::<AtomicPtr<u8>>() }.load(Ordering::Acquire);
            self.first = NonNull::new(next);
        }
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

    pub(crate) fn push_batch(&self, list: &RemoteList) {
        let head = list.first.expect("non-empty remote list");
        // Link this batch in front of whatever is currently published.
        let prev = self.head.swap(head.as_ptr(), Ordering::AcqRel);
        Self::next_of(list.last.as_ptr()).store(prev, Ordering::Release);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Detach the entire pending chain. Single-consumer only.
    ///
    /// Across publishes the order is LIFO; within a batch it stays FIFO. Empty → `None`.
    pub(crate) fn drain(&self) -> Option<RemoteList> {
        let first_ptr = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        let first = NonNull::new(first_ptr)?;
        let mut last = first;
        loop {
            let next = Self::next_of(last.as_ptr()).load(Ordering::Acquire);
            let Some(next) = NonNull::new(next) else {
                break;
            };
            last = next;
        }
        Some(RemoteList::from_ends(first, last))
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::AtomicPtr;

    use super::*;

    #[repr(C)]
    struct TestNode {
        next: AtomicPtr<u8>,
    }

    fn node_ptr(node: &TestNode) -> NonNull<u8> {
        NonNull::new(core::ptr::from_ref(node).cast::<u8>().cast_mut()).unwrap()
    }

    fn collect_list(list: RemoteList) -> [Option<NonNull<u8>>; 4] {
        let mut out = [None; 4];
        for (i, ptr) in list.enumerate() {
            out[i] = Some(ptr);
        }
        out
    }

    #[test]
    fn inbox_push_drain_single() {
        let inbox = Inbox::new();
        let node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
        let ptr = node_ptr(&node);
        inbox.push_batch(&RemoteList::from_ends(ptr, ptr));
        let list = inbox.drain().unwrap();
        assert_eq!(list.first, Some(ptr));
        assert_eq!(list.last, ptr);
        assert_eq!(collect_list(list), [Some(ptr), None, None, None]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_push_drain_lifo_across_batches() {
        let inbox = Inbox::new();
        let first_node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
        let second_node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
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
        let first_node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
        let second_node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
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
        let node = TestNode {
            next: AtomicPtr::new(ptr::null_mut()),
        };
        let ptr = node_ptr(&node);
        moved.push_batch(&RemoteList::from_ends(ptr, ptr));
        let list = moved.drain().unwrap();
        assert_eq!(list.first, Some(ptr));
        assert_eq!(list.last, ptr);
        assert!(moved.is_empty());
    }
}
