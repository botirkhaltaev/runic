//! Cross-CPU drain.
//!
//! Upstream: tcmalloc `FenceCpu` / `Drain` (`tcmalloc/internal/percpu.cc`,
//! `docs/rseq.md`). Local ops are rseq; a remote thread sets stop, fences
//! that CPU, then touches the per-CPU word with a plain atomic. Word ops
//! never fence — this file is why `Rseq::fence` exists.
//!
//! Each rseq number has a non-rseq pair: `SeqCst` fence, steal without
//! membarrier, and per-CPU `AtomicUsize` add under a stealer.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::{CpuId, Error, Rseq, Thread, Words};

fn pin(cpu: u32) -> bool {
    let Ok(cpu) = usize::try_from(cpu) else {
        return false;
    };
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) == 0
    }
}

fn setup() -> Option<(Rseq, Thread, Words)> {
    let rseq = Rseq::try_new()?;
    let thread = rseq.bind()?;
    let cpu = thread.cpu_id()?;
    if !pin(cpu.get()) {
        return None;
    }
    Some((rseq, rseq.bind()?, rseq.words()?))
}

fn workers(cpus: u32) -> usize {
    usize::try_from(cpus)
        .unwrap_or(1)
        .saturating_mul(2)
        .clamp(2, 16)
}

fn add(thread: Thread, words: &Words, count: usize) -> usize {
    loop {
        let Some(cpu) = thread.cpu_id() else {
            continue;
        };
        let Some(w) = words.get(cpu) else {
            continue;
        };
        match thread.fetch_add(w, count) {
            Ok(prev) => return prev,
            Err(Error::Abort | Error::Miss(_)) => {}
        }
    }
}

fn fanin(
    rseq: Rseq,
    words: &Arc<Words>,
    n: usize,
    total: u64,
    work: impl Fn(Thread, &Words) + Send + Sync + 'static,
) -> Duration {
    let n = n.max(1);
    let Ok(total) = usize::try_from(total) else {
        return Duration::ZERO;
    };
    let per = total.div_ceil(n);
    let start = Arc::new(Barrier::new(n + 1));
    let stop = Arc::new(Barrier::new(n + 1));
    let work = Arc::new(work);
    let mut joins = Vec::with_capacity(n);
    for _ in 0..n {
        let words = Arc::clone(words);
        let start = Arc::clone(&start);
        let stop = Arc::clone(&stop);
        let work = Arc::clone(&work);
        joins.push(thread::spawn(move || {
            let Some(thread) = rseq.bind() else {
                start.wait();
                stop.wait();
                return;
            };
            start.wait();
            for _ in 0..per {
                work(thread, &words);
            }
            stop.wait();
        }));
    }
    start.wait();
    let t0 = Instant::now();
    stop.wait();
    let dt = t0.elapsed();
    for j in joins {
        if j.join().is_err() {
            return Duration::ZERO;
        }
    }
    dt
}

fn steal_atomic(words: &[AtomicUsize]) {
    for w in words {
        black_box(w.swap(0, Ordering::Relaxed));
    }
}

fn fanin_plain(n: usize, total: u64, work: impl Fn() + Send + Sync + 'static) -> Duration {
    let n = n.max(1);
    let Ok(total) = usize::try_from(total) else {
        return Duration::ZERO;
    };
    let per = total.div_ceil(n);
    let start = Arc::new(Barrier::new(n + 1));
    let stop = Arc::new(Barrier::new(n + 1));
    let work = Arc::new(work);
    let mut joins = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Arc::clone(&start);
        let stop = Arc::clone(&stop);
        let work = Arc::clone(&work);
        joins.push(thread::spawn(move || {
            start.wait();
            for _ in 0..per {
                work();
            }
            stop.wait();
        }));
    }
    start.wait();
    let t0 = Instant::now();
    stop.wait();
    let dt = t0.elapsed();
    for j in joins {
        if j.join().is_err() {
            return Duration::ZERO;
        }
    }
    dt
}

fn steal_all(rseq: Rseq, words: &Words) {
    for raw in 0..rseq.cpus() {
        let Some(cpu) = CpuId::new(raw) else {
            continue;
        };
        let _ = rseq.fence(cpu);
        let Some(w) = words.get(cpu) else {
            continue;
        };
        // SAFETY: `w` is a live word in `words`.
        black_box(unsafe { w.as_ptr().as_ref() }.swap(0, Ordering::Relaxed));
    }
}

fn fence_one(c: &mut Criterion) {
    let Some((rseq, thread, _)) = setup() else {
        return;
    };
    let Some(cpu) = thread.cpu_id() else {
        return;
    };
    c.bench_function("drain/fence", |b| {
        b.iter(|| black_box(rseq.fence(cpu)));
    });
    c.bench_function("drain/seqcst_fence", |b| {
        b.iter(|| {
            std::sync::atomic::fence(Ordering::SeqCst);
            black_box(cpu);
        });
    });
}

fn steal(c: &mut Criterion) {
    let Some((rseq, _, words)) = setup() else {
        return;
    };
    c.bench_function("drain/steal_all", |b| {
        b.iter(|| steal_all(rseq, &words));
    });
    let atomic: Vec<AtomicUsize> = (0..rseq.cpus()).map(|_| AtomicUsize::new(0)).collect();
    c.bench_function("drain/steal_all_atomic", |b| {
        b.iter(|| steal_atomic(&atomic));
    });
}

fn add_under_steal(c: &mut Criterion) {
    let Some(rseq) = Rseq::try_new() else {
        return;
    };
    let Some(words) = rseq.words() else {
        return;
    };
    let words = Arc::new(words);
    let n = workers(rseq.cpus());

    c.bench_function("drain/add_under_steal", |b| {
        let words = Arc::clone(&words);
        b.iter_custom(|iters| {
            let stop = Arc::new(AtomicBool::new(false));
            let words_d = Arc::clone(&words);
            let stop_d = Arc::clone(&stop);
            let drain = thread::spawn(move || {
                while !stop_d.load(Ordering::Relaxed) {
                    steal_all(rseq, &words_d);
                }
            });
            let dt = fanin(rseq, &words, n, iters, |t, w| {
                black_box(add(t, w, 1));
            });
            stop.store(true, Ordering::Relaxed);
            if drain.join().is_err() {
                return Duration::ZERO;
            }
            dt
        });
    });
    let per_cpu: Arc<Vec<AtomicUsize>> =
        Arc::new((0..rseq.cpus()).map(|_| AtomicUsize::new(0)).collect());
    c.bench_function("drain/add_under_steal_atomic", |b| {
        let per_cpu = Arc::clone(&per_cpu);
        b.iter_custom(|iters| {
            let stop = Arc::new(AtomicBool::new(false));
            let steal_words = Arc::clone(&per_cpu);
            let stop_d = Arc::clone(&stop);
            let drain = thread::spawn(move || {
                while !stop_d.load(Ordering::Relaxed) {
                    steal_atomic(&steal_words);
                }
            });
            let dt = fanin_plain(n, iters, {
                let per_cpu = Arc::clone(&per_cpu);
                move || {
                    black_box(per_cpu[0].fetch_add(1, Ordering::Relaxed));
                }
            });
            stop.store(true, Ordering::Relaxed);
            if drain.join().is_err() {
                return Duration::ZERO;
            }
            dt
        });
    });
}

criterion_group!(benches, fence_one, steal, add_under_steal);
criterion_main!(benches);
