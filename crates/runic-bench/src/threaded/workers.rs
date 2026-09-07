use std::{
    ptr::NonNull,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

/// Per-round work description broadcast to every worker.
#[derive(Clone, Copy)]
pub struct Round {
    pub ops: usize,
    pub live: usize,
}

enum Cmd {
    Run(Round),
    Shutdown,
}

/// Pointer wrapper for `mpsc` between worker threads.
#[derive(Clone, Copy)]
pub struct SendPtr(pub NonNull<u8>);

unsafe impl Send for SendPtr {}

/// Persistent worker pool: spawn once, drive [`Self::round`], join on drop.
///
/// `factory(index)` runs on the worker thread and returns the per-round body.
pub struct Workers {
    cmds: Vec<mpsc::Sender<Cmd>>,
    checksum: Arc<AtomicUsize>,
    done: Arc<Barrier>,
    joins: Vec<JoinHandle<()>>,
}

impl Workers {
    /// # Panics
    ///
    /// Panics if `n` is zero or a worker fails to start.
    #[must_use]
    pub fn spawn<F, W>(n: usize, factory: F) -> Self
    where
        F: Fn(usize) -> W + Send + Clone + 'static,
        W: FnMut(Round) -> usize + 'static,
    {
        assert!(n >= 1, "Workers need at least one thread");
        let checksum = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(Barrier::new(n + 1));
        let mut cmds = Vec::with_capacity(n);
        let mut joins = Vec::with_capacity(n);

        for index in 0..n {
            let (tx, rx) = mpsc::channel();
            cmds.push(tx);
            let checksum = Arc::clone(&checksum);
            let done = Arc::clone(&done);
            let factory = factory.clone();
            joins.push(thread::spawn(move || {
                let mut worker = factory(index);
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Shutdown => break,
                        Cmd::Run(round) => {
                            let local = worker(round);
                            checksum.fetch_xor(local, Ordering::Relaxed);
                            done.wait();
                        }
                    }
                }
            }));
        }

        Self {
            cmds,
            checksum,
            done,
            joins,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.cmds.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Broadcasts `round` to every worker and XOR-folds their checksums.
    ///
    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn round(&self, round: Round) -> usize {
        self.checksum.store(0, Ordering::Relaxed);
        for tx in &self.cmds {
            tx.send(Cmd::Run(round)).unwrap();
        }
        self.done.wait();
        self.checksum.load(Ordering::Relaxed)
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        for tx in &self.cmds {
            let _ = tx.send(Cmd::Shutdown);
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}
