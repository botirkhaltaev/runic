use std::{
    alloc::Layout,
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::target::AllocatorTarget;
use crate::threaded::workers::SendPtr;

/// Measures producer→remote-free→producer-reuse round trips.
///
/// The freer is a continuous drain (not a barrier round), so this is not a [`super::Workers`] pool.
pub struct RemoteReuse {
    stop: Arc<AtomicBool>,
    freer_tx: mpsc::Sender<SendPtr>,
    reuse_ns: Arc<AtomicUsize>,
    rounds: Arc<AtomicUsize>,
    last_round_ns: Arc<AtomicUsize>,
    last_round_ops: Arc<AtomicUsize>,
    join: Option<JoinHandle<()>>,
    target: AllocatorTarget,
    layout: Layout,
}

impl RemoteReuse {
    /// # Panics
    ///
    /// Panics on layout or spawn failure.
    #[must_use]
    pub fn spawn(target: AllocatorTarget) -> Self {
        let layout = Layout::from_size_align(64, 8).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (freer_tx, freer_rx) = mpsc::channel::<SendPtr>();
        let reuse_ns = Arc::new(AtomicUsize::new(0));
        let rounds = Arc::new(AtomicUsize::new(0));
        let last_round_ns = Arc::new(AtomicUsize::new(0));
        let last_round_ops = Arc::new(AtomicUsize::new(0));

        let stop_freer = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !stop_freer.load(Ordering::Acquire) {
                match freer_rx.recv_timeout(Duration::from_millis(1)) {
                    Ok(SendPtr(ptr)) => {
                        target.dealloc(ptr, layout);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            while let Ok(SendPtr(ptr)) = freer_rx.try_recv() {
                target.dealloc(ptr, layout);
            }
        });

        Self {
            stop,
            freer_tx,
            reuse_ns,
            rounds,
            last_round_ns,
            last_round_ops,
            join: Some(join),
            target,
            layout,
        }
    }

    /// Runs `ops` allocate→remote-free→allocate reuse probes on the calling thread.
    ///
    /// # Panics
    ///
    /// Panics if `live` is zero or the freer channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize, live: usize) -> usize {
        assert!(live >= 1, "live depth must be non-zero");
        let mut checksum = 0_usize;
        let mut total_ns = 0_usize;
        let mut hits = 0_usize;

        for i in 0..(live - 1) {
            let ptr = self.target.alloc(black_box(self.layout));
            unsafe { ptr.as_ptr().write(byte(i)) };
            checksum ^= ptr.as_ptr() as usize;
            self.freer_tx.send(SendPtr(ptr)).unwrap();
        }

        for i in 0..ops {
            let first = self.target.alloc(black_box(self.layout));
            unsafe { first.as_ptr().write(byte(i.wrapping_add(live))) };
            checksum ^= first.as_ptr() as usize;
            let start = Instant::now();
            self.freer_tx.send(SendPtr(first)).unwrap();
            let mut spun = 0_usize;
            let spin_limit = 64.max(live.saturating_mul(4));
            loop {
                let next = self.target.alloc(black_box(self.layout));
                unsafe { next.as_ptr().write(byte(i ^ spun)) };
                checksum ^= next.as_ptr() as usize;
                spun += 1;
                if next == first {
                    hits += 1;
                    self.target.dealloc(next, self.layout);
                    break;
                }
                self.target.dealloc(next, self.layout);
                if spun >= spin_limit {
                    break;
                }
            }
            total_ns += usize::try_from(start.elapsed().as_nanos()).unwrap_or(usize::MAX);
        }

        self.last_round_ns.store(total_ns, Ordering::Relaxed);
        self.last_round_ops.store(ops, Ordering::Relaxed);
        self.reuse_ns.fetch_add(total_ns, Ordering::Relaxed);
        self.rounds.fetch_add(ops, Ordering::Relaxed);
        checksum ^ total_ns ^ hits
    }

    /// Mean reuse latency across all completed rounds since spawn / last reset.
    #[must_use]
    pub fn mean_reuse_ns(&self) -> Option<u64> {
        let rounds = self.rounds.load(Ordering::Relaxed);
        if rounds == 0 {
            return None;
        }
        Some((self.reuse_ns.load(Ordering::Relaxed) / rounds) as u64)
    }

    /// Total reuse nanoseconds measured in the most recent [`Self::run_round`] call.
    #[must_use]
    pub fn last_round_reuse_ns(&self) -> Option<u64> {
        let ops = self.last_round_ops.load(Ordering::Relaxed);
        if ops == 0 {
            return None;
        }
        Some(self.last_round_ns.load(Ordering::Relaxed) as u64)
    }
}

impl Drop for RemoteReuse {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
