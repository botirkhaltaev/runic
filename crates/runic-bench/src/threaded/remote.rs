use std::{
    alloc::Layout,
    hint::black_box,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, Ordering},
    },
};

use crate::target::AllocatorTarget;
use crate::threaded::workers::{Round, Workers};

/// Channel-free remote free: owner fills a shared pointer array; freers drain slices.
pub struct RemoteFree {
    target: AllocatorTarget,
    layout: Layout,
    capacity_per_freer: usize,
    slots: Arc<[AtomicPtr<u8>]>,
    workers: Workers,
}

impl RemoteFree {
    /// Already-bound freers draining owner-filled slices without per-element `mpsc`.
    #[must_use]
    pub fn spawn_bound(target: AllocatorTarget, freers: usize) -> Self {
        Self::spawn(target, freers, true)
    }

    /// Never-bound freers draining owner-filled slices without per-element `mpsc`.
    #[must_use]
    pub fn spawn_unbound(target: AllocatorTarget, freers: usize) -> Self {
        Self::spawn(target, freers, false)
    }

    /// # Panics
    ///
    /// Panics if `freers` is zero, layout construction fails, or spawn fails.
    #[must_use]
    fn spawn(target: AllocatorTarget, freers: usize, bind_freers: bool) -> Self {
        assert!(freers >= 1);
        let capacity_per_freer = 2_048;
        let layout = Layout::from_size_align(64, 8).unwrap();
        let total = freers
            .checked_mul(capacity_per_freer)
            .expect("remote free slot capacity overflow");
        let slots: Arc<[AtomicPtr<u8>]> = (0..total)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect::<Vec<_>>()
            .into();

        let workers = Workers::spawn(freers, {
            let slots = Arc::clone(&slots);
            move |freer_index| {
                if bind_freers {
                    let binder = target.alloc(layout);
                    unsafe { binder.as_ptr().write(0xbd) };
                    target.dealloc(binder, layout);
                }
                let slots = Arc::clone(&slots);
                move |round: Round| {
                    let ops = round.ops;
                    let base = freer_index * capacity_per_freer;
                    let mut local = 0_usize;
                    for offset in 0..ops {
                        let ptr = slots[base + offset].swap(ptr::null_mut(), Ordering::Acquire);
                        debug_assert!(!ptr.is_null());
                        local ^= ptr as usize;
                        let ptr = std::ptr::NonNull::new(ptr).unwrap();
                        target.dealloc(ptr, layout);
                    }
                    local
                }
            }
        });

        Self {
            target,
            layout,
            capacity_per_freer,
            slots,
            workers,
        }
    }

    /// Owner-thread allocate into the shared array. Not part of the timed free phase.
    ///
    /// # Panics
    ///
    /// Panics if `ops` exceeds the per-freer capacity or allocation fails.
    #[must_use]
    pub fn prepare_round(&self, ops: usize) -> usize {
        assert!(
            ops <= self.capacity_per_freer,
            "ops {ops} exceeds capacity {}",
            self.capacity_per_freer
        );
        let mut checksum = 0_usize;
        let freers = self.workers.len();
        for freer_index in 0..freers {
            let base = freer_index * self.capacity_per_freer;
            for offset in 0..ops {
                let ptr = self.target.alloc(black_box(self.layout));
                unsafe { ptr.as_ptr().write(byte(freer_index ^ offset)) };
                checksum ^= ptr.as_ptr() as usize;
                self.slots[base + offset].store(ptr.as_ptr(), Ordering::Release);
            }
        }
        checksum
    }

    /// Freers drain their disjoint slices. Call only after [`Self::prepare_round`].
    #[must_use]
    pub fn run_free_round(&self, ops: usize) -> usize {
        self.workers.round(Round { ops, live: 1 })
    }

    /// Owner-thread alloc/free that drains published remote frees via flush/accept.
    #[must_use]
    pub fn run_accept_round(&self, ops: usize) -> usize {
        let mut checksum = 0_usize;
        for i in 0..ops {
            let ptr = self.target.alloc(black_box(self.layout));
            unsafe { ptr.as_ptr().write(byte(i)) };
            checksum ^= ptr.as_ptr() as usize;
            self.target.dealloc(ptr, self.layout);
        }
        black_box(checksum)
    }
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
