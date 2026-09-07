use std::{
    alloc::Layout,
    hint::black_box,
    sync::{Arc, Mutex, mpsc},
};

use crate::target::AllocatorTarget;
use crate::threaded::workers::{Round, SendPtr, Workers};

/// Owner performs local alloc/free churn while freers concurrently remote-free owner blocks.
pub struct OwnerConcurrent {
    workers: Workers,
}

impl OwnerConcurrent {
    /// `freers` remote freers; the owner is an extra worker.
    ///
    /// # Panics
    ///
    /// Panics if `freers` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, freers: usize) -> Self {
        assert!(freers >= 1);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let mut senders = Vec::with_capacity(freers);
        let mut receivers = Vec::with_capacity(freers);
        for _ in 0..freers {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }
        let receivers = Arc::new(Mutex::new(receivers));
        let senders = Arc::new(senders);

        Self {
            workers: Workers::spawn(freers + 1, {
                let receivers = Arc::clone(&receivers);
                let senders = Arc::clone(&senders);
                move |index| {
                    let rx = receivers
                        .lock()
                        .unwrap()
                        .get_mut(index)
                        .and_then(Option::take);
                    let owner = (index == freers).then(|| Arc::clone(&senders));
                    let mut pending = Vec::new();
                    move |round: Round| {
                        if let Some(rx) = &rx {
                            assert!(round.live >= 1);
                            pending.clear();
                            pending.reserve(round.live);
                            let mut local = 0_usize;
                            for _ in 0..round.ops {
                                let received = rx.recv().unwrap().0;
                                local ^= received.as_ptr() as usize;
                                pending.push(received);
                                if pending.len() == round.live {
                                    let old = pending.remove(0);
                                    target.dealloc(old, layout);
                                }
                            }
                            for old in pending.drain(..) {
                                target.dealloc(old, layout);
                            }
                            local
                        } else if let Some(senders) = &owner {
                            let mut local = 0_usize;
                            for i in 0..round.ops {
                                let local_ptr = target.alloc(black_box(layout));
                                unsafe { local_ptr.as_ptr().write(byte(i)) };
                                local ^= local_ptr.as_ptr() as usize;
                                target.dealloc(local_ptr, layout);

                                for (freer, tx) in senders.iter().enumerate() {
                                    let remote = target.alloc(black_box(layout));
                                    unsafe { remote.as_ptr().write(byte(i ^ freer)) };
                                    local ^= remote.as_ptr() as usize;
                                    tx.send(SendPtr(remote)).unwrap();
                                }
                            }
                            local
                        } else {
                            0
                        }
                    }
                }
            }),
        }
    }

    /// Owner performs `ops` local churn steps and feeds `ops` blocks to each freer.
    ///
    /// # Panics
    ///
    /// Panics if `live` is zero or a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize, live: usize) -> usize {
        assert!(live >= 1, "live depth must be non-zero");
        self.workers.round(Round { ops, live })
    }
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
