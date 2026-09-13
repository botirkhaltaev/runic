//! Per-CPU intrusive stack.
//!
//! Upstream: librseq `basic_percpu_ops_test.c` (per-CPU list), librseq
//! mempool free list (`src/rseq-mempool.c`). Store `next` on a
//! thread-owned node, then `cmpeqv_storev` the head. Two committing
//! stores in one CS is not restartable — that was #135.
//!
//! Isolated and fan-in vs an `AtomicUsize` Treiber stack and a TLS `Cell`
//! head. Non-rseq benches run even if rseq is unavailable.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::{Error, Rseq, Thread, Words};

const EMPTY: usize = 0;

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

fn cas(thread: Thread, words: &Words, expect: usize, new: usize) -> Result<usize, Error> {
    loop {
        let Some(cpu) = thread.cpu_id() else {
            continue;
        };
        let Some(w) = words.get(cpu) else {
            continue;
        };
        match thread.compare_exchange(w, expect, new) {
            Err(Error::Abort) => {}
            other => return other,
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

struct List {
    next: Vec<AtomicUsize>,
}

impl List {
    fn with_nodes(n: usize) -> Self {
        Self {
            next: (0..=n).map(|_| AtomicUsize::new(EMPTY)).collect(),
        }
    }

    fn pop_rseq(&self, thread: Thread, words: &Words) -> usize {
        loop {
            let head = add(thread, words, 0);
            if head == EMPTY {
                return EMPTY;
            }
            let nxt = self.next[head].load(Ordering::Relaxed);
            match cas(thread, words, head, nxt) {
                Ok(_) => return head,
                Err(Error::Miss(_) | Error::Abort) => {}
            }
        }
    }

    fn push_rseq(&self, thread: Thread, words: &Words, node: usize) {
        loop {
            let head = add(thread, words, 0);
            self.next[node].store(head, Ordering::Relaxed);
            match cas(thread, words, head, node) {
                Ok(_) => return,
                Err(Error::Miss(cur)) => self.next[node].store(cur, Ordering::Relaxed),
                Err(Error::Abort) => {}
            }
        }
    }

    fn pop_atomic(&self, head: &AtomicUsize) -> usize {
        loop {
            let cur = head.load(Ordering::Relaxed);
            if cur == EMPTY {
                return EMPTY;
            }
            let nxt = self.next[cur].load(Ordering::Relaxed);
            if head
                .compare_exchange(cur, nxt, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return cur;
            }
        }
    }

    fn push_atomic(&self, head: &AtomicUsize, node: usize) {
        loop {
            let cur = head.load(Ordering::Relaxed);
            self.next[node].store(cur, Ordering::Relaxed);
            if head
                .compare_exchange(cur, node, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    fn pop_tls(&self, head: &Cell<usize>) -> usize {
        let cur = head.get();
        if cur == EMPTY {
            return EMPTY;
        }
        head.set(self.next[cur].load(Ordering::Relaxed));
        cur
    }

    fn push_tls(&self, head: &Cell<usize>, node: usize) {
        self.next[node].store(head.get(), Ordering::Relaxed);
        head.set(node);
    }
}

fn prime_rseq(thread: Thread, words: &Words, list: &List, nodes: impl Iterator<Item = usize>) {
    for node in nodes {
        list.push_rseq(thread, words, node);
    }
}

fn isolated(c: &mut Criterion) {
    if let Some((thread, words)) = setup() {
        let list = List::with_nodes(2);
        prime_rseq(thread, &words, &list, std::iter::once(1));
        c.bench_function("freelist/rseq_pop_push", |b| {
            b.iter(|| {
                let n = list.pop_rseq(thread, &words);
                black_box(n);
                if n != EMPTY {
                    list.push_rseq(thread, &words, n);
                }
            });
        });
    }

    let atomic_head = AtomicUsize::new(EMPTY);
    let atomic_list = List::with_nodes(2);
    atomic_list.push_atomic(&atomic_head, 1);
    c.bench_function("freelist/atomic_pop_push", |b| {
        b.iter(|| {
            let n = atomic_list.pop_atomic(&atomic_head);
            black_box(n);
            if n != EMPTY {
                atomic_list.push_atomic(&atomic_head, n);
            }
        });
    });

    let tls_head = Cell::new(EMPTY);
    let tls_list = List::with_nodes(2);
    tls_list.push_tls(&tls_head, 1);
    c.bench_function("freelist/tls_pop_push", |b| {
        b.iter(|| {
            let n = tls_list.pop_tls(&tls_head);
            black_box(n);
            if n != EMPTY {
                tls_list.push_tls(&tls_head, n);
            }
        });
    });
}

fn fanin_benches(c: &mut Criterion) {
    let n = workers();
    let nodes = n.saturating_mul(4).max(8);

    if let Some(rseq) = Rseq::try_new()
        && let Some(words) = rseq.words()
    {
        let words = Arc::new(words);
        let list = Arc::new(List::with_nodes(nodes));
        if let Some(thread) = rseq.bind() {
            prime_rseq(thread, &words, &list, 1..=nodes);
        }
        c.bench_function("freelist/fanin_rseq", |b| {
            let words = Arc::clone(&words);
            let list = Arc::clone(&list);
            b.iter_custom(|iters| {
                fanin(rseq, &words, n, iters, {
                    let list = Arc::clone(&list);
                    move |t, w| {
                        let node = list.pop_rseq(t, w);
                        if node != EMPTY {
                            list.push_rseq(t, w, node);
                        }
                        black_box(node);
                    }
                })
            });
        });
    }

    let atomic_head = Arc::new(AtomicUsize::new(EMPTY));
    let atomic_list = Arc::new(List::with_nodes(nodes));
    for node in 1..=nodes {
        atomic_list.push_atomic(&atomic_head, node);
    }
    c.bench_function("freelist/fanin_atomic", |b| {
        let list = Arc::clone(&atomic_list);
        let head = Arc::clone(&atomic_head);
        b.iter_custom(|iters| {
            fanin_plain(n, iters, {
                let list = Arc::clone(&list);
                let head = Arc::clone(&head);
                move || {
                    let node = list.pop_atomic(&head);
                    if node != EMPTY {
                        list.push_atomic(&head, node);
                    }
                    black_box(node);
                }
            })
        });
    });
    c.bench_function("freelist/fanin_tls", |b| {
        b.iter_custom(|iters| {
            fanin_plain(n, iters, || {
                thread_local!(static HEAD: Cell<usize> = const { Cell::new(EMPTY) });
                thread_local!(static OWNED: Cell<usize> = const { Cell::new(1) });
                HEAD.with(|head| {
                    OWNED.with(|owned| {
                        let node = owned.get();
                        if head.get() == EMPTY {
                            head.set(node);
                        }
                        let n = {
                            let cur = head.get();
                            if cur == EMPTY {
                                EMPTY
                            } else {
                                head.set(EMPTY);
                                cur
                            }
                        };
                        if n != EMPTY {
                            head.set(n);
                        }
                        black_box(n);
                    });
                });
            })
        });
    });
}

criterion_group!(benches, isolated, fanin_benches);
criterion_main!(benches);
