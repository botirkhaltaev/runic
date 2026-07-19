use crate::heap::HeapId;

/// Unified ownership for allocator entities that can be owned by either the central heap or a thread-local heap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Owner {
    /// Owned by the central/root heap (global, shared).
    Central,
    /// Owned by a specific thread-local heap.
    Thread(HeapId),
}

impl Owner {
    /// Creates an Owner for the given heap: Central if heap is root, Thread otherwise.
    pub(crate) const fn for_heap(heap: HeapId) -> Self {
        if heap.is_root() {
            Self::Central
        } else {
            Self::Thread(heap)
        }
    }

    /// Returns true if this is the Central owner.
    pub(crate) const fn is_central(self) -> bool {
        matches!(self, Self::Central)
    }
}
