//! Isolated word-op pair. Pin the process to CPU 0 when recording.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::Rseq;

fn tls_cell(c: &mut Criterion) {
    let cell = Cell::new(0usize);
    c.bench_function("tls_cell_add", |b| {
        b.iter(|| {
            let v = cell.get();
            cell.set(v.wrapping_add(black_box(1)));
        });
    });
}

fn tls_cell_cas(c: &mut Criterion) {
    let cell = Cell::new(0usize);
    c.bench_function("tls_cell_cas", |b| {
        b.iter(|| {
            let v = cell.get();
            if v == black_box(v) {
                cell.set(v.wrapping_add(1));
            }
        });
    });
}

fn rseq_add(c: &mut Criterion) {
    let Some(rseq) = Rseq::try_new() else {
        return;
    };
    let Some(thread) = rseq.bind() else {
        return;
    };
    let Some(cpu) = thread.cpu_id() else {
        return;
    };
    let Some(words) = rseq.words() else {
        return;
    };
    let Some(w) = words.get(cpu) else {
        return;
    };
    c.bench_function("rseq_add", |b| {
        b.iter(|| {
            black_box(thread.fetch_add(w, black_box(1)));
        });
    });
}

fn rseq_cas(c: &mut Criterion) {
    let Some(rseq) = Rseq::try_new() else {
        return;
    };
    let Some(thread) = rseq.bind() else {
        return;
    };
    let Some(cpu) = thread.cpu_id() else {
        return;
    };
    let Some(words) = rseq.words() else {
        return;
    };
    let Some(w) = words.get(cpu) else {
        return;
    };
    let mut expect = 0usize;
    c.bench_function("rseq_cas", |b| {
        b.iter(|| {
            let cur = expect;
            black_box(
                thread
                    .compare_exchange(w, cur, cur.wrapping_add(1))
                    .unwrap(),
            );
            expect = cur.wrapping_add(1);
        });
    });
}

fn atomic_add(c: &mut Criterion) {
    let word = AtomicUsize::new(0);
    c.bench_function("atomic_add", |b| {
        b.iter(|| {
            black_box(word.fetch_add(black_box(1), Ordering::Relaxed));
        });
    });
}

fn atomic_cas(c: &mut Criterion) {
    let word = AtomicUsize::new(0);
    c.bench_function("atomic_cas", |b| {
        b.iter(|| {
            let cur = word.load(Ordering::Relaxed);
            let _ = black_box(word.compare_exchange(
                cur,
                cur.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ));
        });
    });
}

criterion_group!(
    benches,
    tls_cell,
    tls_cell_cas,
    rseq_add,
    rseq_cas,
    atomic_add,
    atomic_cas
);
criterion_main!(benches);
