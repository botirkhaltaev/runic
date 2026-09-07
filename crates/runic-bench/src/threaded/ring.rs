use std::{
    alloc::Layout,
    hint::black_box,
    sync::{Arc, Mutex, mpsc},
};

use crate::target::AllocatorTarget;
use crate::threaded::workers::{Round, SendPtr, Workers};

/// Persistent cross-thread free ring with a configurable live-set depth.
pub struct FreeRing {
    workers: Workers,
}

impl FreeRing {
    /// # Panics
    ///
    /// Panics if `threads < 2` or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        assert!(threads >= 2);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }
        let receivers = Arc::new(Mutex::new(receivers));
        let senders = Arc::new(senders);

        Self {
            workers: Workers::spawn(threads, {
                let receivers = Arc::clone(&receivers);
                let senders = Arc::clone(&senders);
                move |index| {
                    let rx = receivers.lock().unwrap()[index].take().unwrap();
                    let tx = senders[(index + 1) % threads].clone();
                    let mut outstanding = Vec::new();
                    move |round: Round| {
                        outstanding.clear();
                        outstanding.reserve(round.live);
                        let mut checksum = 0_usize;
                        for i in 0..round.ops {
                            let ptr = target.alloc(black_box(layout));
                            unsafe { ptr.as_ptr().write(byte(i)) };
                            tx.send(SendPtr(ptr)).unwrap();
                            let received = rx.recv().unwrap().0;
                            checksum ^= received.as_ptr() as usize;
                            outstanding.push(received);
                            if outstanding.len() == round.live {
                                let old = outstanding.remove(0);
                                target.dealloc(old, layout);
                            }
                        }
                        for old in outstanding.drain(..) {
                            target.dealloc(old, layout);
                        }
                        checksum
                    }
                }
            }),
        }
    }

    /// # Panics
    ///
    /// Panics if `live` is zero or a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize, live: usize) -> usize {
        assert!(live >= 1);
        self.workers.round(Round { ops, live })
    }
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
