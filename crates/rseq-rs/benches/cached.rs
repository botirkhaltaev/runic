//! One cached object per CPU.
//!
//! Upstream: tcmalloc per-CPU cache front (`docs/rseq.md`,
//! `tcmalloc/internal/percpu_tcmalloc.h`). Production slabs update a
//! `current`/`end` header plus a slot — that is a dedicated CS, not two
//! word ops. v0.1 expresses the 1-deep case: `cmpeqv_storev` take/put of
//! a single pointer word (the cached-block shape).
//!
//! Isolated take+put vs TLS `Cell` and `AtomicUsize` CAS. `cached/rseq_take_put`
//! is the real caller (re-read `cpu_id` + `get` each iter).
//! `cached/rseq_word_take_put` pins, `get`s once, and reuses that `Word`.
//! Pin with `taskset -c 0`. Non-rseq benches run even if rseq is unavailable.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::{Error, Rseq, Thread, Word, Words};

const OBJ: usize = 1;

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

fn take_put(thread: Thread, words: &Words) {
    loop {
        match cas(thread, words, OBJ, 0) {
            Ok(_) => break,
            Err(Error::Miss(_) | Error::Abort) => {}
        }
    }
    loop {
        match cas(thread, words, 0, OBJ) {
            Ok(_) => return,
            Err(Error::Miss(_) | Error::Abort) => {}
        }
    }
}

fn cas_word(thread: Thread, word: Word<'_>, expect: usize, new: usize) -> Result<usize, Error> {
    loop {
        match thread.compare_exchange(word, expect, new) {
            Err(Error::Abort) => {}
            other => return other,
        }
    }
}

fn take_put_word(thread: Thread, word: Word<'_>) {
    loop {
        match cas_word(thread, word, OBJ, 0) {
            Ok(_) => break,
            Err(Error::Miss(_) | Error::Abort) => {}
        }
    }
    loop {
        match cas_word(thread, word, 0, OBJ) {
            Ok(_) => return,
            Err(Error::Miss(_) | Error::Abort) => {}
        }
    }
}

fn isolated(c: &mut Criterion) {
    if let Some((thread, words)) = setup() {
        assert_eq!(cas(thread, &words, 0, OBJ), Ok(0));
        c.bench_function("cached/rseq_take_put", |b| {
            b.iter(|| {
                take_put(thread, &words);
                black_box(OBJ);
            });
        });
        if let Some(cpu) = thread.cpu_id()
            && let Some(word) = words.get(cpu)
        {
            c.bench_function("cached/rseq_word_take_put", |b| {
                b.iter(|| {
                    take_put_word(thread, word);
                    black_box(OBJ);
                });
            });
        }
    }
    let cell = Cell::new(OBJ);
    let atomic = AtomicUsize::new(OBJ);
    c.bench_function("cached/tls_take_put", |b| {
        b.iter(|| {
            let v = cell.get();
            cell.set(0);
            black_box(v);
            cell.set(OBJ);
        });
    });
    c.bench_function("cached/atomic_take_put", |b| {
        b.iter(|| {
            let _ = atomic.compare_exchange(OBJ, 0, Ordering::Relaxed, Ordering::Relaxed);
            let _ = atomic.compare_exchange(0, OBJ, Ordering::Relaxed, Ordering::Relaxed);
            black_box(OBJ);
        });
    });
}

criterion_group!(benches, isolated);
criterion_main!(benches);
