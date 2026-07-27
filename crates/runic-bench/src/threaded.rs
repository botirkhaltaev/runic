use std::{
    alloc::Layout,
    hint::black_box,
    ptr,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{allocator_target::AllocatorTarget, rng::TraceRng, workload};

struct SendPtr(*mut u8);

unsafe impl Send for SendPtr {}

/// Runs per-thread local allocation churn.
///
/// Spawns and joins workers inside the call — setup/lifecycle noise included.
/// Prefer [`PersistentLocalChurn`] for allocator-path profiles.
///
/// # Panics
///
/// Panics if a worker thread panics.
#[must_use]
pub fn thread_local_churn(target: AllocatorTarget, threads: usize, ops_per_thread: usize) -> usize {
    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        for index in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                workload::single_size_churn(target, 64 + index * 8, ops_per_thread)
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

/// Sends allocations around a thread ring and frees them on another thread.
///
/// Spawns and joins workers inside the call — setup/lifecycle noise included.
/// Prefer [`PersistentCrossThreadRing`] for allocator-path profiles.
///
/// # Panics
///
/// Panics if layout construction, allocation, channel operations, or thread joins fail.
#[must_use]
pub fn cross_thread_free_ring(
    target: AllocatorTarget,
    threads: usize,
    ops_per_thread: usize,
) -> usize {
    let layout = Layout::from_size_align(64, 8).unwrap();

    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);

        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }

        let mut handles = Vec::with_capacity(threads);
        for index in 0..threads {
            let tx = senders[(index + 1) % threads].clone();
            let rx = receivers[index].take().unwrap();
            let barrier = Arc::clone(&barrier);

            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut checksum = 0_usize;
                for i in 0..ops_per_thread {
                    let ptr = target.alloc(black_box(layout));
                    unsafe { ptr.as_ptr().write(byte(i)) };
                    tx.send(SendPtr(ptr.as_ptr())).unwrap();
                    let received = rx.recv().unwrap();
                    checksum ^= received.0 as usize;
                    let ptr = std::ptr::NonNull::new(received.0).unwrap();
                    target.dealloc(ptr, layout);
                }
                checksum
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

/// Runs randomized small-allocation traces on multiple threads.
///
/// Spawns and joins workers inside the call — setup/lifecycle noise included.
///
/// # Panics
///
/// Panics if a worker thread panics.
#[must_use]
pub fn mixed_thread_random(
    target: AllocatorTarget,
    threads: usize,
    ops_per_thread: usize,
) -> usize {
    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        for index in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut rng = TraceRng::new(0x9e37_79b9_7f4a_7c15 ^ index as u64);
                let ops = ops_per_thread + rng.next_usize(8);
                workload::small_biased_random(target, rng.next_u64(), ops, 128)
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

/// Spawns threads, runs one remote-free burst, joins — measures drain/unbind noise.
///
/// # Panics
///
/// Panics on layout, channel, or join failure.
#[must_use]
pub fn draining_late_free(target: AllocatorTarget, threads: usize, ops_per_thread: usize) -> usize {
    cross_thread_free_ring(target, threads, ops_per_thread)
}

enum WorkerCmd {
    Run { ops: usize, live: usize },
    Shutdown,
}

/// Persistent local-churn workers: spawn once, drive rounds without rejoin.
pub struct PersistentLocalChurn {
    cmds: Vec<mpsc::Sender<WorkerCmd>>,
    results: Arc<Mutex<Vec<usize>>>,
    done: Arc<Barrier>,
    joins: Vec<JoinHandle<()>>,
}

impl PersistentLocalChurn {
    /// # Panics
    ///
    /// Panics if a worker fails to start.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        let results = Arc::new(Mutex::new(vec![0_usize; threads]));
        let done = Arc::new(Barrier::new(threads + 1));
        let mut cmds = Vec::with_capacity(threads);
        let mut joins = Vec::with_capacity(threads);

        for index in 0..threads {
            let (tx, rx) = mpsc::channel();
            cmds.push(tx);
            let results = Arc::clone(&results);
            let done = Arc::clone(&done);
            joins.push(thread::spawn(move || {
                let size = 64 + index * 8;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        WorkerCmd::Shutdown => break,
                        WorkerCmd::Run { ops, live: _ } => {
                            let checksum = workload::single_size_churn(target, size, ops);
                            results.lock().unwrap()[index] = checksum;
                            done.wait();
                        }
                    }
                }
            }));
        }

        Self {
            cmds,
            results,
            done,
            joins,
        }
    }

    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        for tx in &self.cmds {
            tx.send(WorkerCmd::Run { ops, live: 1 }).unwrap();
        }
        self.done.wait();
        self.results.lock().unwrap().iter().sum()
    }
}

impl Drop for PersistentLocalChurn {
    fn drop(&mut self) {
        for tx in &self.cmds {
            let _ = tx.send(WorkerCmd::Shutdown);
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Persistent cross-thread free ring with a configurable live-set depth.
pub struct PersistentCrossThreadRing {
    cmds: Vec<mpsc::Sender<WorkerCmd>>,
    results: Arc<Mutex<Vec<usize>>>,
    done: Arc<Barrier>,
    joins: Vec<JoinHandle<()>>,
}

impl PersistentCrossThreadRing {
    /// # Panics
    ///
    /// Panics on channel or spawn failure.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        assert!(threads >= 2);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let results = Arc::new(Mutex::new(vec![0_usize; threads]));
        let done = Arc::new(Barrier::new(threads + 1));

        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }

        let mut cmds = Vec::with_capacity(threads);
        let mut joins = Vec::with_capacity(threads);

        for index in 0..threads {
            let (cmd_tx, cmd_rx) = mpsc::channel();
            cmds.push(cmd_tx);
            let tx = senders[(index + 1) % threads].clone();
            let rx = receivers[index].take().unwrap();
            let results = Arc::clone(&results);
            let done = Arc::clone(&done);

            joins.push(thread::spawn(move || {
                let mut outstanding: Vec<*mut u8> = Vec::new();
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        WorkerCmd::Shutdown => break,
                        WorkerCmd::Run { ops, live } => {
                            outstanding.clear();
                            outstanding.reserve(live);
                            let mut checksum = 0_usize;
                            for i in 0..ops {
                                let ptr = target.alloc(black_box(layout));
                                unsafe { ptr.as_ptr().write(byte(i)) };
                                tx.send(SendPtr(ptr.as_ptr())).unwrap();
                                let received = rx.recv().unwrap().0;
                                checksum ^= received as usize;
                                outstanding.push(received);
                                if outstanding.len() == live {
                                    let old = outstanding.remove(0);
                                    let ptr = std::ptr::NonNull::new(old).unwrap();
                                    target.dealloc(ptr, layout);
                                }
                            }
                            for old in outstanding.drain(..) {
                                let ptr = std::ptr::NonNull::new(old).unwrap();
                                target.dealloc(ptr, layout);
                            }
                            results.lock().unwrap()[index] = checksum;
                            done.wait();
                        }
                    }
                }
            }));
        }

        Self {
            cmds,
            results,
            done,
            joins,
        }
    }

    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize, live: usize) -> usize {
        assert!(live >= 1);
        for tx in &self.cmds {
            tx.send(WorkerCmd::Run { ops, live }).unwrap();
        }
        self.done.wait();
        self.results.lock().unwrap().iter().sum()
    }
}

impl Drop for PersistentCrossThreadRing {
    fn drop(&mut self) {
        for tx in &self.cmds {
            let _ = tx.send(WorkerCmd::Shutdown);
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

/// One allocator producer, many freer threads — remote frees fan into the producer heap.
pub struct PersistentRemoteFanIn {
    alloc_cmd: mpsc::Sender<WorkerCmd>,
    free_cmds: Vec<mpsc::Sender<WorkerCmd>>,
    checksum: Arc<AtomicUsize>,
    done: Arc<Barrier>,
    joins: Vec<JoinHandle<()>>,
}

impl PersistentRemoteFanIn {
    /// `threads` is the freer count; the producer is an extra thread.
    ///
    /// # Panics
    ///
    /// Panics on spawn or channel failure.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, freers: usize) -> Self {
        assert!(freers >= 1);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let checksum = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(Barrier::new(freers + 2));

        let mut free_senders = Vec::with_capacity(freers);
        let mut free_receivers = Vec::with_capacity(freers);
        for _ in 0..freers {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            free_senders.push(tx);
            free_receivers.push(Some(rx));
        }

        let mut joins = Vec::with_capacity(freers + 1);
        let mut free_cmds = Vec::with_capacity(freers);

        for rx_slot in &mut free_receivers {
            let (cmd_tx, cmd_rx) = mpsc::channel();
            free_cmds.push(cmd_tx);
            let rx = rx_slot.take().unwrap();
            let done = Arc::clone(&done);
            let checksum = Arc::clone(&checksum);
            joins.push(thread::spawn(move || {
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        WorkerCmd::Shutdown => break,
                        WorkerCmd::Run { ops, live: _ } => {
                            let mut local = 0_usize;
                            for _ in 0..ops {
                                let received = rx.recv().unwrap().0;
                                local ^= received as usize;
                                let ptr = std::ptr::NonNull::new(received).unwrap();
                                target.dealloc(ptr, layout);
                            }
                            checksum.fetch_xor(local, Ordering::Relaxed);
                            done.wait();
                        }
                    }
                }
            }));
        }

        let (alloc_cmd, alloc_rx) = mpsc::channel();
        let free_senders = Arc::new(free_senders);
        let done_alloc = Arc::clone(&done);
        let checksum_alloc = Arc::clone(&checksum);
        joins.push(thread::spawn(move || {
            while let Ok(cmd) = alloc_rx.recv() {
                match cmd {
                    WorkerCmd::Shutdown => break,
                    WorkerCmd::Run { ops, live: _ } => {
                        let freers = free_senders.len();
                        let mut local = 0_usize;
                        for i in 0..ops * freers {
                            let ptr = target.alloc(black_box(layout));
                            unsafe { ptr.as_ptr().write(byte(i)) };
                            local ^= ptr.as_ptr() as usize;
                            free_senders[i % freers]
                                .send(SendPtr(ptr.as_ptr()))
                                .unwrap();
                        }
                        checksum_alloc.fetch_xor(local, Ordering::Relaxed);
                        done_alloc.wait();
                    }
                }
            }
        }));

        Self {
            alloc_cmd,
            free_cmds,
            checksum,
            done,
            joins,
        }
    }

    /// Each freer performs `ops` frees; producer allocates `ops * freers` blocks.
    ///
    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.checksum.store(0, Ordering::Relaxed);
        for tx in &self.free_cmds {
            tx.send(WorkerCmd::Run { ops, live: 1 }).unwrap();
        }
        self.alloc_cmd
            .send(WorkerCmd::Run { ops, live: 1 })
            .unwrap();
        self.done.wait();
        self.checksum.load(Ordering::Relaxed)
    }
}

impl Drop for PersistentRemoteFanIn {
    fn drop(&mut self) {
        let _ = self.alloc_cmd.send(WorkerCmd::Shutdown);
        for tx in &self.free_cmds {
            let _ = tx.send(WorkerCmd::Shutdown);
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Owner allocates locally while freers concurrently remote-free the owner's blocks.
pub struct PersistentOwnerConcurrent {
    inner: PersistentRemoteFanIn,
}

impl PersistentOwnerConcurrent {
    /// # Panics
    ///
    /// Panics on spawn failure.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, freers: usize) -> Self {
        Self {
            inner: PersistentRemoteFanIn::spawn(target, freers),
        }
    }

    /// # Panics
    ///
    /// Panics if a worker channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.inner.run_round(ops)
    }
}

/// Measures producer→remote-free→producer-reuse round trips.
pub struct PersistentRemoteReuse {
    stop: Arc<AtomicBool>,
    freer_tx: mpsc::Sender<SendPtr>,
    reuse_ns: Arc<AtomicUsize>,
    rounds: Arc<AtomicUsize>,
    join: Option<JoinHandle<()>>,
    target: AllocatorTarget,
    layout: Layout,
}

impl PersistentRemoteReuse {
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

        let stop_freer = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !stop_freer.load(Ordering::Acquire) {
                match freer_rx.recv_timeout(Duration::from_millis(1)) {
                    Ok(SendPtr(ptr)) => {
                        let ptr = std::ptr::NonNull::new(ptr).unwrap();
                        target.dealloc(ptr, layout);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            while let Ok(SendPtr(ptr)) = freer_rx.try_recv() {
                let ptr = std::ptr::NonNull::new(ptr).unwrap();
                target.dealloc(ptr, layout);
            }
        });

        Self {
            stop,
            freer_tx,
            reuse_ns,
            rounds,
            join: Some(join),
            target,
            layout,
        }
    }

    /// Runs `ops` allocate→remote-free→allocate reuse probes on the calling thread.
    ///
    /// `live` is the freer backlog depth: `live - 1` unmeasured remote frees are primed
    /// once, then each sample measures send→reuse with that backlog in flight (`1`
    /// matches the previous single-outstanding path).
    ///
    /// # Panics
    ///
    /// Panics if `live` is zero or the freer channel is closed.
    #[must_use]
    pub fn run_round(&self, ops: usize, live: usize) -> usize {
        assert!(live >= 1, "live depth must be non-zero");
        let mut checksum = 0_usize;
        let mut total_ns = 0_usize;

        for i in 0..(live - 1) {
            let ptr = self.target.alloc(black_box(self.layout));
            unsafe { ptr.as_ptr().write(byte(i)) };
            checksum ^= ptr.as_ptr() as usize;
            self.freer_tx.send(SendPtr(ptr.as_ptr())).unwrap();
        }

        for i in 0..ops {
            let first = self.target.alloc(black_box(self.layout));
            unsafe { first.as_ptr().write(byte(i.wrapping_add(live))) };
            checksum ^= first.as_ptr() as usize;
            let start = Instant::now();
            self.freer_tx.send(SendPtr(first.as_ptr())).unwrap();
            // Spin-alloc until the freer has returned capacity via remote inbox flush.
            // Touching the new block keeps the work from being optimized away.
            let mut spun = 0_usize;
            loop {
                let next = self.target.alloc(black_box(self.layout));
                unsafe { next.as_ptr().write(byte(i ^ spun)) };
                checksum ^= next.as_ptr() as usize;
                spun += 1;
                if next.as_ptr() == first.as_ptr() || spun >= 64.max(live) {
                    self.target.dealloc(next, self.layout);
                    break;
                }
                self.target.dealloc(next, self.layout);
            }
            total_ns += start.elapsed().as_nanos() as usize;
        }

        self.reuse_ns.fetch_add(total_ns, Ordering::Relaxed);
        self.rounds.fetch_add(ops, Ordering::Relaxed);
        checksum ^ total_ns
    }

    #[must_use]
    pub fn mean_reuse_ns(&self) -> Option<u64> {
        let rounds = self.rounds.load(Ordering::Relaxed);
        if rounds == 0 {
            return None;
        }
        Some((self.reuse_ns.load(Ordering::Relaxed) / rounds) as u64)
    }
}

impl Drop for PersistentRemoteReuse {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

enum ChannelFreeCmd {
    FreeRound,
    Shutdown,
}

/// Channel-free remote free: owner fills a shared pointer array; freers drain slices.
///
/// Bound mode binds each freer TLS once at spawn (`claim → TLS batch → publish`).
/// Unbound mode never binds freers (singleton publish path).
pub struct PersistentChannelFreeRemote {
    target: AllocatorTarget,
    layout: Layout,
    capacity_per_freer: usize,
    slots: Arc<[AtomicPtr<u8>]>,
    ops_per_freer: Arc<AtomicUsize>,
    checksum: Arc<AtomicUsize>,
    done: Arc<Barrier>,
    cmds: Vec<mpsc::Sender<ChannelFreeCmd>>,
    joins: Vec<JoinHandle<()>>,
}

/// Bound freers: channel-free `claim → TLS batch → publish`.
pub type PersistentBoundRemoteBatch = PersistentChannelFreeRemote;

/// Never-bound freers: channel-free singleton publish path.
pub type PersistentUnboundRemoteSingleton = PersistentChannelFreeRemote;

impl PersistentChannelFreeRemote {
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

    /// Spawns `freers` workers. When `bind_freers` is true, each freer allocates once
    /// on its own thread before accepting free rounds.
    ///
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
        let ops_per_freer = Arc::new(AtomicUsize::new(0));
        let checksum = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(Barrier::new(freers + 1));
        let ready = Arc::new(Barrier::new(freers + 1));

        let mut cmds = Vec::with_capacity(freers);
        let mut joins = Vec::with_capacity(freers);

        for freer_index in 0..freers {
            let (tx, rx) = mpsc::channel();
            cmds.push(tx);
            let slots = Arc::clone(&slots);
            let ops_per_freer = Arc::clone(&ops_per_freer);
            let checksum = Arc::clone(&checksum);
            let done = Arc::clone(&done);
            let ready = Arc::clone(&ready);
            joins.push(thread::spawn(move || {
                if bind_freers {
                    let binder = target.alloc(layout);
                    unsafe { binder.as_ptr().write(0xbd) };
                    target.dealloc(binder, layout);
                }
                // Freers park until the controller finishes spawn setup.
                ready.wait();
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        ChannelFreeCmd::Shutdown => break,
                        ChannelFreeCmd::FreeRound => {
                            let ops = ops_per_freer.load(Ordering::Acquire);
                            let base = freer_index * capacity_per_freer;
                            let mut local = 0_usize;
                            for offset in 0..ops {
                                let ptr =
                                    slots[base + offset].swap(ptr::null_mut(), Ordering::Acquire);
                                debug_assert!(!ptr.is_null());
                                local ^= ptr as usize;
                                let ptr = std::ptr::NonNull::new(ptr).unwrap();
                                target.dealloc(ptr, layout);
                            }
                            checksum.fetch_xor(local, Ordering::Relaxed);
                            done.wait();
                        }
                    }
                }
            }));
        }

        ready.wait();

        Self {
            target,
            layout,
            capacity_per_freer,
            slots,
            ops_per_freer,
            checksum,
            done,
            cmds,
            joins,
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
        let freers = self.cmds.len();
        for freer_index in 0..freers {
            let base = freer_index * self.capacity_per_freer;
            for offset in 0..ops {
                let ptr = self.target.alloc(black_box(self.layout));
                unsafe { ptr.as_ptr().write(byte(freer_index ^ offset)) };
                checksum ^= ptr.as_ptr() as usize;
                self.slots[base + offset].store(ptr.as_ptr(), Ordering::Release);
            }
        }
        self.ops_per_freer.store(ops, Ordering::Release);
        checksum
    }

    /// Freers drain their disjoint slices. Call only after [`Self::prepare_round`].
    ///
    /// # Panics
    ///
    /// Panics if a freer command channel is closed.
    #[must_use]
    pub fn run_free_round(&self) -> usize {
        self.checksum.store(0, Ordering::Relaxed);
        for tx in &self.cmds {
            tx.send(ChannelFreeCmd::FreeRound).unwrap();
        }
        self.done.wait();
        self.checksum.load(Ordering::Relaxed)
    }
}

impl Drop for PersistentChannelFreeRemote {
    fn drop(&mut self) {
        for tx in &self.cmds {
            let _ = tx.send(ChannelFreeCmd::Shutdown);
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
