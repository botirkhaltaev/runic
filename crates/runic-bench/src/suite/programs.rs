use std::{hint::black_box, time::Instant};

use criterion::{BenchmarkId, Criterion, Throughput};

use crate::{programs, suite::elems, target::AllocatorTarget};

pub fn register(c: &mut Criterion, targets: &[AllocatorTarget]) {
    register_one(c, targets, "programs/larson", |target, threads| {
        let workers = programs::Larson::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
    register_one(c, targets, "programs/xmalloc", |target, threads| {
        let workers = programs::Xmalloc::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
    register_one(c, targets, "programs/cache_thrash", |target, threads| {
        let workers = programs::CacheThrash::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
    register_one(c, targets, "programs/cache_scratch", |target, threads| {
        let workers = programs::CacheScratch::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
    register_one(c, targets, "programs/sh6bench", |target, threads| {
        let workers = programs::Sh6bench::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
    register_one(c, targets, "programs/cfrac", |target, threads| {
        let workers = programs::Cfrac::spawn(target, threads);
        move || workers.run_round(programs::OPS)
    });
}

fn register_one<F, R>(c: &mut Criterion, targets: &[AllocatorTarget], group_name: &str, spawn: F)
where
    F: Fn(AllocatorTarget, usize) -> R + Copy,
    R: Fn() -> usize,
{
    let mut group = c.benchmark_group(group_name);

    for &target in targets {
        for &threads in programs::THREADS {
            group.throughput(Throughput::Elements(elems(threads * programs::OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let run = spawn(target, threads);
                        let start = Instant::now();
                        for _ in 0..iters {
                            black_box(run());
                        }
                        let elapsed = start.elapsed();
                        drop(run);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}
