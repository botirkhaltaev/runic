use std::{
    alloc::Layout,
    hint::black_box,
    ptr::NonNull,
    sync::{Arc, Mutex, mpsc},
};

use crate::{
    rng::TraceRng,
    target::AllocatorTarget,
    threaded::{Round, SendPtr, Workers},
};

pub const THREADS: &[usize] = &[1, 4, 16];
pub const OPS: usize = 2_048;
const LARSON_SLOTS: usize = 256;
const SH6_LIVE: usize = 512;
const SH6_BATCH: usize = 32;
const CACHE_LINES: usize = 256;
const CACHE_LINE: usize = 64;
const SCRATCH_SIZE: usize = 8;

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

fn touch(ptr: NonNull<u8>, size: usize, tag: usize) {
    unsafe {
        ptr.as_ptr().write(byte(tag));
        if size > 1 {
            ptr.as_ptr().add(size - 1).write(byte(tag >> 8));
        }
    }
}

struct Slots {
    target: AllocatorTarget,
    slots: Vec<Option<(SendPtr, Layout)>>,
}

impl Slots {
    fn new(target: AllocatorTarget, n: usize) -> Self {
        Self {
            target,
            slots: vec![None; n],
        }
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        for (SendPtr(ptr), item) in self.slots.drain(..).flatten() {
            self.target.dealloc(ptr, item);
        }
    }
}

/// Per-thread random-size slot arrays; after each round the arrays rotate so the
/// next thread frees what another allocated.
pub struct Larson {
    workers: Workers,
}

impl Larson {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        assert!(threads >= 1);
        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<Vec<Option<(SendPtr, Layout)>>>();
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
                    let mut rng = TraceRng::new(0x1a25_07f1_c0de_u64 ^ index as u64);
                    let mut slots = Slots::new(target, LARSON_SLOTS);
                    move |round: Round| {
                        let mut checksum = 0_usize;
                        for i in 0..round.ops {
                            let slot = rng.next_usize(LARSON_SLOTS);
                            if let Some((SendPtr(ptr), old)) = slots.slots[slot].take() {
                                checksum ^= ptr.as_ptr() as usize;
                                target.dealloc(ptr, old);
                            }
                            let size = rng.biased_size(2048);
                            let item = layout(size);
                            let ptr = target.alloc(black_box(item));
                            touch(ptr, size, i);
                            checksum ^= ptr.as_ptr() as usize;
                            slots.slots[slot] = Some((SendPtr(ptr), item));
                        }
                        if threads > 1 {
                            tx.send(std::mem::take(&mut slots.slots)).unwrap();
                            slots.slots = rx.recv().unwrap();
                        }
                        checksum
                    }
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round {
            ops,
            live: LARSON_SLOTS,
        })
    }
}

/// Producer/consumer: producers allocate random sizes, consumers free them.
pub struct Xmalloc {
    workers: Workers,
}

impl Xmalloc {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        assert!(threads >= 1);
        let consumers = threads / 2;
        let mut senders = Vec::with_capacity(consumers.max(1));
        let mut receivers = Vec::with_capacity(consumers.max(1));
        for _ in 0..consumers.max(1) {
            let (tx, rx) = mpsc::sync_channel::<(SendPtr, Layout)>(64);
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
                    let is_consumer = consumers > 0 && index < consumers;
                    let rx = is_consumer.then(|| receivers.lock().unwrap()[index].take().unwrap());
                    let senders = Arc::clone(&senders);
                    let mut rng = TraceRng::new(0x5861_5f0e_u64 ^ index as u64);
                    move |round: Round| {
                        if let Some(rx) = &rx {
                            let mut local = 0_usize;
                            for _ in 0..round.ops {
                                let (SendPtr(ptr), item) = rx.recv().unwrap();
                                local ^= ptr.as_ptr() as usize;
                                target.dealloc(ptr, item);
                            }
                            local
                        } else {
                            let n = senders.len();
                            let mut local = 0_usize;
                            for i in 0..round.ops {
                                let size = rng.biased_size(4096);
                                let item = layout(size);
                                let ptr = target.alloc(black_box(item));
                                touch(ptr, size, i);
                                local ^= ptr.as_ptr() as usize;
                                if consumers == 0 {
                                    target.dealloc(ptr, item);
                                } else {
                                    senders[i % n].send((SendPtr(ptr), item)).unwrap();
                                }
                            }
                            local
                        }
                    }
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round { ops, live: 1 })
    }
}

/// Each thread allocates its own cache-line objects and writes them repeatedly.
pub struct CacheThrash {
    workers: Workers,
}

impl CacheThrash {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        let item = Layout::from_size_align(CACHE_LINE, CACHE_LINE).unwrap();
        Self {
            workers: Workers::spawn(threads, move |_| {
                let mut lines = Vec::with_capacity(CACHE_LINES);
                move |round: Round| {
                    lines.clear();
                    let mut checksum = 0_usize;
                    for i in 0..CACHE_LINES {
                        let ptr = target.alloc(black_box(item));
                        touch(ptr, CACHE_LINE, i);
                        lines.push(ptr);
                    }
                    for i in 0..round.ops {
                        for (k, ptr) in lines.iter().enumerate() {
                            touch(*ptr, CACHE_LINE, i ^ k);
                            checksum ^= unsafe { ptr.as_ptr().read() } as usize;
                        }
                    }
                    for ptr in lines.drain(..) {
                        target.dealloc(ptr, item);
                    }
                    checksum
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round {
            ops,
            live: CACHE_LINES,
        })
    }
}

/// One thread allocates packed 8-byte objects and hands them out — false sharing.
pub struct CacheScratch {
    workers: Workers,
}

impl CacheScratch {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        let item = layout(SCRATCH_SIZE);
        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }
        let (back_tx, back_rx) = mpsc::channel::<SendPtr>();
        let receivers = Arc::new(Mutex::new(receivers));
        let senders = Arc::new(senders);
        let back_rx = Arc::new(Mutex::new(Some(back_rx)));

        Self {
            workers: Workers::spawn(threads, {
                let receivers = Arc::clone(&receivers);
                let senders = Arc::clone(&senders);
                let back_rx = Arc::clone(&back_rx);
                move |index| {
                    let rx = receivers.lock().unwrap()[index].take().unwrap();
                    let back_rx = (index == 0).then(|| back_rx.lock().unwrap().take().unwrap());
                    let back_tx = back_tx.clone();
                    let senders = Arc::clone(&senders);
                    move |round: Round| {
                        if index == 0 {
                            for tx in senders.iter() {
                                let ptr = target.alloc(black_box(item));
                                touch(ptr, SCRATCH_SIZE, 1);
                                tx.send(SendPtr(ptr)).unwrap();
                            }
                        }
                        let ptr = rx.recv().unwrap().0;
                        let mut checksum = 0_usize;
                        for i in 0..round.ops {
                            touch(ptr, SCRATCH_SIZE, i);
                            checksum ^= unsafe { ptr.as_ptr().read() } as usize;
                        }
                        if let Some(back_rx) = &back_rx {
                            target.dealloc(ptr, item);
                            for _ in 1..threads {
                                let SendPtr(other) = back_rx.recv().unwrap();
                                target.dealloc(other, item);
                            }
                        } else {
                            back_tx.send(SendPtr(ptr)).unwrap();
                        }
                        checksum
                    }
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round { ops, live: 1 })
    }
}

/// Rolling live set, mixed 8 B–64 KiB bands, batch alloc / free half / periodic release.
pub struct Sh6bench {
    workers: Workers,
}

impl Sh6bench {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        Self {
            workers: Workers::spawn(threads, move |index| {
                let mut rng = TraceRng::new(0x586_b0a1_u64 ^ index as u64);
                let mut live: Vec<(NonNull<u8>, Layout)> = Vec::with_capacity(SH6_LIVE);
                move |round: Round| {
                    let mut checksum = 0_usize;
                    let mut id = 0_usize;
                    for step in 0..round.ops {
                        for _ in 0..SH6_BATCH {
                            if live.len() >= SH6_LIVE {
                                break;
                            }
                            let size = rng.biased_size(64 * 1024);
                            let item = layout(size);
                            let ptr = target.alloc(black_box(item));
                            touch(ptr, size, id);
                            checksum ^= ptr.as_ptr() as usize;
                            live.push((ptr, item));
                            id += 1;
                        }
                        if live.len() >= SH6_LIVE / 2 {
                            let drop_n = live.len() / 2;
                            for (ptr, item) in live.drain(..drop_n) {
                                checksum ^= ptr.as_ptr() as usize;
                                target.dealloc(ptr, item);
                            }
                        }
                        if step.is_multiple_of(8) {
                            for (ptr, item) in live.drain(..) {
                                checksum ^= ptr.as_ptr() as usize;
                                target.dealloc(ptr, item);
                            }
                        }
                    }
                    for (ptr, item) in live.drain(..) {
                        target.dealloc(ptr, item);
                    }
                    checksum
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round {
            ops,
            live: SH6_LIVE,
        })
    }
}

/// Tiny 8–128 B nested LIFO lifetimes (cfrac-style short-lived trees).
pub struct Cfrac {
    workers: Workers,
}

impl Cfrac {
    /// # Panics
    ///
    /// Panics if `threads` is zero or spawn fails.
    #[must_use]
    pub fn spawn(target: AllocatorTarget, threads: usize) -> Self {
        Self {
            workers: Workers::spawn(threads, move |index| {
                let mut rng = TraceRng::new(0xc_f1ac_u64 ^ index as u64);
                let mut stack = Vec::with_capacity(16);
                move |round: Round| {
                    let mut checksum = 0_usize;
                    for i in 0..round.ops {
                        let depth = 4 + rng.next_usize(12);
                        for d in 0..depth {
                            let size = 8 + rng.next_usize(121);
                            let item = layout(size);
                            let ptr = target.alloc(black_box(item));
                            touch(ptr, size, i ^ d);
                            stack.push((ptr, item));
                        }
                        while let Some((ptr, item)) = stack.pop() {
                            checksum ^= ptr.as_ptr() as usize;
                            target.dealloc(ptr, item);
                        }
                    }
                    checksum
                }
            }),
        }
    }

    #[must_use]
    pub fn run_round(&self, ops: usize) -> usize {
        self.workers.round(Round { ops, live: 1 })
    }
}

/// Spawn, one round, join — used by the metrics binary.
#[must_use]
pub fn larson(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = Larson::spawn(target, threads);
    workers.run_round(ops)
}

#[must_use]
pub fn xmalloc(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = Xmalloc::spawn(target, threads);
    workers.run_round(ops)
}

#[must_use]
pub fn cache_thrash(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = CacheThrash::spawn(target, threads);
    workers.run_round(ops)
}

#[must_use]
pub fn cache_scratch(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = CacheScratch::spawn(target, threads);
    workers.run_round(ops)
}

#[must_use]
pub fn sh6bench(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = Sh6bench::spawn(target, threads);
    workers.run_round(ops)
}

#[must_use]
pub fn cfrac(target: AllocatorTarget, threads: usize, ops: usize) -> usize {
    let workers = Cfrac::spawn(target, threads);
    workers.run_round(ops)
}
