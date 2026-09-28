//! Freed memory held back before reuse. `hardened` only.
//!
//! One list per heap, one byte budget for both kinds. Blocks queue through
//! their run's link word, extents through their slot link. Blocks leave
//! first; an extent leaves only when no block is waiting, because a block
//! returns to its freelist without a lock and an extent needs the extent heap.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::allocator::Allocator;
use crate::heap::extent::Extent;
use crate::heap::list::LinkedList;
use crate::heap::run::Run;

/// Bytes held back before the oldest are really freed.
const BUDGET: usize = 256 * 1024;

/// Owner-exclusive delay list with a byte count freers may read.
pub(crate) struct Delay {
    /// Owner writes, reclaim loads. Release / Acquire.
    bytes: AtomicUsize,
    /// Owner thread only, same rule as `Heap::thread_id`.
    lists: UnsafeCell<Lists>,
}

struct Lists {
    /// FIFO through each block's link word. `head` pops, `tail` appends.
    head: Option<NonNull<u8>>,
    tail: Option<NonNull<u8>>,
    extents: LinkedList<'static, Extent>,
    extent_bytes: usize,
}

// SAFETY: `bytes` is atomic. `lists` is written only by the thread that has
// the owning heap on its list, which is the `Heap` ownership rule.
unsafe impl Sync for Delay {}

impl Delay {
    pub(crate) const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            lists: UnsafeCell::new(Lists {
                head: None,
                tail: None,
                extents: LinkedList::new(),
                extent_bytes: 0,
            }),
        }
    }

    /// Bytes still held. Reclaim treats a non-zero count as live.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }

    pub(crate) fn over_budget(&self) -> bool {
        self.bytes.load(Ordering::Relaxed) > BUDGET
    }

    /// Extents waiting, as `(count, bytes)`. The cache counts them against its budget.
    pub(crate) fn extents(&self) -> (usize, usize) {
        self.with(|lists| (lists.extents.len(), lists.extent_bytes))
    }

    /// Hold a freed block of `run`, then return the oldest blocks over budget.
    pub(crate) fn hold(&self, run: &Run, block: NonNull<u8>) {
        self.bytes.fetch_add(run.class().size(), Ordering::Release);
        self.with(|lists| {
            run.link_block(block, None);
            match lists.tail {
                Some(tail) => Self::run_of(tail).link_block(tail, Some(block)),
                None => lists.head = Some(block),
            }
            lists.tail = Some(block);
        });
        while self.over_budget() && self.release() {}
    }

    /// Hold a freed extent. The extent heap releases it, since retaining needs the cache.
    pub(crate) fn hold_extent(&self, extent: &'static Extent) {
        let bytes = extent.payload().len();
        self.bytes.fetch_add(bytes, Ordering::Release);
        self.with(|lists| {
            lists.extents.push_back(extent);
            lists.extent_bytes += bytes;
        });
    }

    /// Return the oldest block to its run. `false` when only extents wait.
    pub(crate) fn release(&self) -> bool {
        let Some(block) = self.with(|lists| {
            let block = lists.head?;
            let next = Self::run_of(block).next_block(block);
            lists.head = next;
            if next.is_none() {
                lists.tail = None;
            }
            Some(block)
        }) else {
            return false;
        };
        let run = Self::run_of(block);
        self.bytes.fetch_sub(run.class().size(), Ordering::Release);
        run.push_free(block);
        true
    }

    /// The oldest extent, no longer counted.
    pub(crate) fn take_extent(&self) -> Option<&'static Extent> {
        let extent = self.with(|lists| {
            let extent = lists.extents.pop_front()?;
            lists.extent_bytes -= extent.payload().len();
            Some(extent)
        })?;
        self.bytes
            .fetch_sub(extent.payload().len(), Ordering::Release);
        Some(extent)
    }

    /// Move every held block of `run` onto its freelist. Miss path, once
    /// `extend` cannot serve, and `discard` before it resets the run.
    pub(crate) fn recall(&self, run: &Run) {
        let recalled = self.with(|lists| {
            let mut recalled = 0;
            let mut prev: Option<NonNull<u8>> = None;
            let mut current = lists.head;
            while let Some(block) = current {
                let owner = Self::run_of(block);
                let next = owner.next_block(block);
                if owner == run {
                    match prev {
                        Some(prev) => Self::run_of(prev).link_block(prev, next),
                        None => lists.head = next,
                    }
                    if lists.tail == Some(block) {
                        lists.tail = prev;
                    }
                    run.push_free(block);
                    recalled += 1;
                } else {
                    prev = Some(block);
                }
                current = next;
            }
            recalled
        });
        self.bytes
            .fetch_sub(recalled * run.class().size(), Ordering::Release);
    }

    fn run_of(block: NonNull<u8>) -> &'static Run {
        Run::header_of(block).unwrap_or_else(|| Allocator::abort())
    }

    fn with<R>(&self, body: impl FnOnce(&mut Lists) -> R) -> R {
        // SAFETY: owner-exclusive. Freers only load `bytes`.
        body(unsafe { &mut *self.lists.get() })
    }
}
