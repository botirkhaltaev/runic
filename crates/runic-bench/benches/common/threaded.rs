use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use runic_bench::{allocator_target::AllocatorTarget, threaded};

const THREAD_COUNTS: &[usize] = &[2, 4];
const OPS_PER_THREAD: usize = 512;

fn configure_group(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1));
}

pub fn register(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    register_thread_local_churn(c, suite, targets);
    register_cross_thread_free_ring(c, suite, targets);
    register_mixed_thread_random(c, suite, targets);
}

fn register_thread_local_churn(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/thread_local_churn"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter(|| threaded::thread_local_churn(target, threads, OPS_PER_THREAD));
                },
            );
        }
    }

    group.finish();
}

fn register_cross_thread_free_ring(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/cross_thread_free_ring"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench
                        .iter(|| threaded::cross_thread_free_ring(target, threads, OPS_PER_THREAD));
                },
            );
        }
    }

    group.finish();
}

fn register_mixed_thread_random(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/mixed_thread_random"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter(|| threaded::mixed_thread_random(target, threads, OPS_PER_THREAD));
                },
            );
        }
    }

    group.finish();
}
