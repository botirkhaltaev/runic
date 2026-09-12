//! Isolated pop+push pair. Pin the process to CPU 0 when recording.

use std::cell::Cell;
use std::hint::black_box;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};
use rseq_rs::{CpuId, LockedStacks, Rseq};

fn token(n: usize) -> NonNull<u8> {
    NonNull::new(n as *mut u8).unwrap()
}

fn tls_cell(c: &mut Criterion) {
    struct Stack {
        cur: Cell<u32>,
        slots: [Cell<usize>; 32],
    }
    let s = Stack {
        cur: Cell::new(0),
        slots: [const { Cell::new(0) }; 32],
    };
    c.bench_function("tls_cell_pair", |b| {
        b.iter(|| {
            let i = s.cur.get();
            s.slots[i as usize].set(black_box(1));
            s.cur.set(i + 1);
            let i = s.cur.get() - 1;
            s.cur.set(i);
            black_box(s.slots[i as usize].get());
        });
    });
}

fn locked(c: &mut Criterion) {
    let stacks = LockedStacks::<u8>::new(2, 32).unwrap();
    let cpu = CpuId::new(0).unwrap();
    let p = token(1);
    stacks.push_cpu(cpu, p).unwrap();
    c.bench_function("locked_pair", |b| {
        b.iter(|| {
            let q = stacks.pop_cpu(cpu).unwrap();
            stacks.push_cpu(cpu, q).unwrap();
        });
    });
}

fn rseq_stacks(c: &mut Criterion) {
    let Some(rseq) = Rseq::try_new() else {
        return;
    };
    let Some(thread) = rseq.bind() else {
        return;
    };
    let Some(stacks) = rseq.stacks::<u8>(32) else {
        return;
    };
    let p = token(1);
    stacks.push(&thread, p).unwrap();
    c.bench_function("rseq_pair", |b| {
        b.iter(|| {
            let q = stacks.pop(&thread).unwrap();
            stacks.push(&thread, q).unwrap();
        });
    });
}

fn cas_stack(c: &mut Criterion) {
    let current = AtomicU32::new(0);
    let slots = [const { AtomicU32::new(0) }; 32];
    c.bench_function("atomic_cas_pair", |b| {
        b.iter(|| {
            let i = current.load(Ordering::Relaxed);
            slots[i as usize].store(1, Ordering::Relaxed);
            current
                .compare_exchange(i, i + 1, Ordering::AcqRel, Ordering::Relaxed)
                .unwrap();
            let i = current.load(Ordering::Relaxed) - 1;
            current
                .compare_exchange(i + 1, i, Ordering::AcqRel, Ordering::Relaxed)
                .unwrap();
            black_box(slots[i as usize].load(Ordering::Relaxed));
        });
    });
}

criterion_group!(benches, tls_cell, locked, rseq_stacks, cas_stack);
criterion_main!(benches);
