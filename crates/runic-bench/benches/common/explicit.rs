use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput};
use runic_bench::{allocator_target::AllocatorTarget, workload};

const SMALL_OPS: usize = 512;
const RANDOM_OPS: usize = 2_000;
const RANDOM_LIVE: usize = 256;
const LARGE_OPS: usize = 64;
const REALLOC_ROUNDS: usize = 16;

fn configure_group(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1));
}

pub fn register(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    register_single_size_churn(c, suite, targets);
    register_recycled_live_churn(c, suite, targets);
    register_recycled_live_hotspot(c, suite, targets);
    register_owner_free_only(c, suite, targets);
    register_freelist_allocate_only(c, suite, targets);
    register_size_boundary_sweep(c, suite, targets);
    register_small_biased_random(c, suite, targets);
    register_alignment_stress(c, suite, targets);
    register_realloc_growth(c, suite, targets);
    register_large_alloc_churn(c, suite, targets);
    register_alloc_zeroed(c, suite, targets);
}

fn register_single_size_churn(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/single_size_churn"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        for &size in workload::SINGLE_SIZE_CHURN {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| workload::single_size_churn(target, size, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_recycled_live_churn(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/recycled_live_churn"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        for &size in workload::SIZE_CLASSES {
            for &live in workload::RECYCLED_LIVE_DEPTHS {
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{size}/live:{live}")),
                    &(target, size, live),
                    |bench, &(target, size, live)| {
                        bench.iter(|| {
                            workload::single_size_recycled_churn(target, size, SMALL_OPS, live)
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_recycled_live_hotspot(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/recycled_live_hotspot"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        for &size in workload::LOCAL_HOTSPOT_SIZES {
            for &live in workload::RECYCLED_LIVE_DEPTHS {
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{size}/live:{live}")),
                    &(target, size, live),
                    |bench, &(target, size, live)| {
                        bench.iter(|| {
                            workload::single_size_recycled_churn(target, size, SMALL_OPS, live)
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_owner_free_only(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/owner_free_only"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(workload::PHASE_BATCH as u64));

    for &target in targets {
        for &size in workload::LOCAL_PHASE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter_custom(|iters| {
                        let mut slots = workload::fill_live(target, size, workload::PHASE_BATCH);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            let start = Instant::now();
                            black_box(workload::owner_free_only(target, size, &mut slots));
                            elapsed += start.elapsed();
                            black_box(workload::refill_live(target, size, &mut slots));
                        }
                        for slot in slots {
                            let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
                            target.dealloc(slot, layout);
                        }
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_freelist_allocate_only(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/freelist_allocate_only"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(workload::PHASE_BATCH as u64));

    for &target in targets {
        for &size in workload::LOCAL_PHASE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter_custom(|iters| {
                        let mut slots = workload::fill_live(target, size, workload::PHASE_BATCH);
                        black_box(workload::seed_freelist(target, size, &mut slots));
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            let start = Instant::now();
                            black_box(workload::freelist_allocate_only(target, size, &mut slots));
                            elapsed += start.elapsed();
                            black_box(workload::seed_freelist(target, size, &mut slots));
                        }
                        // Final seed left slots dangling; nothing live to free.
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_size_boundary_sweep(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/size_boundary_sweep"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| workload::size_boundary_sweep(target, SMALL_OPS));
        });
    }

    group.finish();
}

fn register_small_biased_random(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/small_biased_random"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(RANDOM_OPS as u64));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| {
                workload::small_biased_random(
                    target,
                    0xf3ee_a110_c001_cafe,
                    RANDOM_OPS,
                    RANDOM_LIVE,
                )
            });
        });
    }

    group.finish();
}

fn register_alignment_stress(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/alignment_stress"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        for &(size, align) in workload::ALIGNMENT_CASES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), format!("size_{size}_align_{align}")),
                &(target, size, align),
                |bench, &(target, size, align)| {
                    bench.iter(|| workload::alignment_stress(target, size, align, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_realloc_growth(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/realloc_growth"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(REALLOC_ROUNDS as u64));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| workload::realloc_growth(target, REALLOC_ROUNDS));
        });
    }

    group.finish();
}

fn register_large_alloc_churn(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/large_alloc_churn"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(LARGE_OPS as u64));

    for &target in targets {
        for &size in workload::LARGE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| workload::large_alloc_churn(target, size, LARGE_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_alloc_zeroed(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/alloc_zeroed"));
    configure_group(&mut group);
    group.throughput(Throughput::Elements(SMALL_OPS as u64));

    for &target in targets {
        for &size in &[64, 4096, 64 * 1024] {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| workload::alloc_zeroed(target, size, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}
