use crate::{
    micro,
    target::AllocatorTarget,
    threaded::workers::{Round, Workers},
};

/// Persistent per-thread local allocation churn.
pub struct LocalChurn {
    workers: Workers,
}

impl LocalChurn {
    /// # Panics
    ///
    /// Panics if a worker fails to start.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        Self {
            workers: Workers::spawn(threads, move |index| {
                let size = 64 + index * 8;
                move |round: Round| micro::single_size_churn(target, size, round.ops)
            }),
        }
    }

    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round { ops, live: 1 })
    }
}
