//! Per-CPU counter.
//!
//! Upstream: librseq `rseq_addv` (`include/rseq/arch/x86.h`), kernel
//! `percpu_counter`, allocator/stats increments that must share across
//! threads on one CPU.
//!
//! Isolated: one pinned thread. `counter/rseq` is the real caller (re-read
//! `cpu_id` + `get` each iter). `counter/rseq_word` pins, `get`s once, and
//! reuses that `Word` so the CS number is visible. Fan-in: threads > cores
//! on the same words. Non-rseq: shared `AtomicUsize` and TLS `Cell`.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::{Error, Rseq, Thread, Word, Words};

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

fn setup() -> Option<(Thread, Words)> {
    let rseq = Rseq::try_new()?;
    let thread = rseq.bind()?;
    let cpu = thread.cpu_id()?;
    if !pin(cpu.get()) {
        return None;
    }
    Some((rseq.bind()?, rseq.words()?))
}

fn workers() -> usize {
    thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
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

fn add_word(thread: Thread, word: Word<'_>, count: usize) -> usize {
    loop {
        match thread.fetch_add(word, count) {
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

fn isolated(c: &mut Criterion) {
    if let Some((thread, words)) = setup() {
        c.bench_function("counter/rseq", |b| {
            b.iter(|| black_box(add(thread, &words, black_box(1))));
        });
        if let Some(cpu) = thread.cpu_id()
            && let Some(word) = words.get(cpu)
        {
            c.bench_function("counter/rseq_word", |b| {
                b.iter(|| black_box(add_word(thread, word, black_box(1))));
            });
        }
    }
    let atomic = AtomicUsize::new(0);
    let cell = Cell::new(0usize);
    c.bench_function("counter/atomic", |b| {
        b.iter(|| black_box(atomic.fetch_add(black_box(1), Ordering::Relaxed)));
    });
    c.bench_function("counter/tls", |b| {
        b.iter(|| {
            let v = cell.get();
            cell.set(v.wrapping_add(black_box(1)));
            black_box(v);
        });
    });
}

fn fanin_benches(c: &mut Criterion) {
    let n = workers();
    if let Some(rseq) = Rseq::try_new()
        && let Some(words) = rseq.words()
    {
        let words = Arc::new(words);
        c.bench_function("counter/fanin_rseq", |b| {
            let words = Arc::clone(&words);
            b.iter_custom(|iters| {
                fanin(rseq, &words, n, iters, |t, w| {
                    black_box(add(t, w, 1));
                })
            });
        });
    }
    let shared = Arc::new(AtomicUsize::new(0));
    c.bench_function("counter/fanin_atomic", |b| {
        let shared = Arc::clone(&shared);
        b.iter_custom(|iters| {
            fanin_plain(n, iters, {
                let shared = Arc::clone(&shared);
                move || {
                    black_box(shared.fetch_add(1, Ordering::Relaxed));
                }
            })
        });
    });
    c.bench_function("counter/fanin_tls", |b| {
        b.iter_custom(|iters| {
            fanin_plain(n, iters, || {
                thread_local!(static CELL: Cell<usize> = const { Cell::new(0) });
                CELL.with(|cell| {
                    let v = cell.get();
                    cell.set(v.wrapping_add(1));
                    black_box(v);
                });
            })
        });
    });
}

criterion_group!(benches, isolated, fanin_benches);
criterion_main!(benches);
