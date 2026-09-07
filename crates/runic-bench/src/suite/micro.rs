use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};

use crate::{micro, suite::elems, target::AllocatorTarget};

const SMALL_OPS: usize = 512;
const RANDOM_OPS: usize = 2_000;
const RANDOM_LIVE: usize = 256;
const LARGE_OPS: usize = 64;
const REALLOC_ROUNDS: usize = 16;

pub fn register(c: &mut Criterion, targets: &[AllocatorTarget]) {
    register_single_size_churn(c, targets);
    register_recycled_churn(c, targets);
    register_recycled_hotspot(c, targets);
    register_owner_free(c, targets);
    register_freelist_allocate(c, targets);
    register_size_boundary_sweep(c, targets);
    register_small_biased_random(c, targets);
    register_alignment_stress(c, targets);
    register_realloc_growth(c, targets);
    register_large_churn(c, targets);
    register_alloc_zeroed(c, targets);
}

fn register_single_size_churn(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/single_size_churn");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        for &size in micro::SINGLE_SIZE_CHURN {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| micro::single_size_churn(target, size, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_recycled_churn(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/recycled_churn");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        for &size in micro::SIZE_CLASSES {
            for &live in micro::RECYCLED_LIVE_DEPTHS {
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{size}/live:{live}")),
                    &(target, size, live),
                    |bench, &(target, size, live)| {
                        bench.iter(|| micro::recycled_churn(target, size, SMALL_OPS, live));
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_recycled_hotspot(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/recycled_hotspot");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        for &size in micro::LOCAL_HOTSPOT_SIZES {
            for &live in micro::RECYCLED_LIVE_DEPTHS {
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{size}/live:{live}")),
                    &(target, size, live),
                    |bench, &(target, size, live)| {
                        bench.iter(|| micro::recycled_churn(target, size, SMALL_OPS, live));
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_owner_free(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/owner_free");
    group.throughput(Throughput::Elements(elems(micro::PHASE_BATCH)));

    for &target in targets {
        for &size in micro::LOCAL_PHASE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter_batched(
                        || micro::fill(target, size, micro::PHASE_BATCH),
                        micro::free,
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }

    group.finish();
}

fn register_freelist_allocate(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/freelist_allocate");
    group.throughput(Throughput::Elements(elems(micro::PHASE_BATCH)));

    for &target in targets {
        for &size in micro::LOCAL_PHASE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter_batched(
                        || {
                            let mut live = micro::fill(target, size, micro::PHASE_BATCH);
                            micro::seed(&mut live);
                            live
                        },
                        micro::allocate,
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }

    group.finish();
}

fn register_size_boundary_sweep(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/size_boundary_sweep");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| micro::size_boundary_sweep(target, SMALL_OPS));
        });
    }

    group.finish();
}

fn register_small_biased_random(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/small_biased_random");
    group.throughput(Throughput::Elements(elems(RANDOM_OPS)));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| {
                micro::small_biased_random(target, 0xf3ee_a110_c001_cafe, RANDOM_OPS, RANDOM_LIVE)
            });
        });
    }

    group.finish();
}

fn register_alignment_stress(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/alignment_stress");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        for &(size, align) in micro::ALIGNMENT_CASES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), format!("size_{size}_align_{align}")),
                &(target, size, align),
                |bench, &(target, size, align)| {
                    bench.iter(|| micro::alignment_stress(target, size, align, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_realloc_growth(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/realloc_growth");
    group.throughput(Throughput::Elements(elems(REALLOC_ROUNDS)));

    for &target in targets {
        group.bench_function(target.name(), |bench| {
            bench.iter(|| micro::realloc_growth(target, REALLOC_ROUNDS));
        });
    }

    group.finish();
}

fn register_large_churn(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/large_churn");
    group.throughput(Throughput::Elements(elems(LARGE_OPS)));

    for &target in targets {
        for &size in micro::LARGE_SIZES {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| micro::large_churn(target, size, LARGE_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_alloc_zeroed(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("micro/alloc_zeroed");
    group.throughput(Throughput::Elements(elems(SMALL_OPS)));

    for &target in targets {
        for &size in &[64, 4096, 64 * 1024] {
            group.bench_with_input(
                BenchmarkId::new(target.name(), size),
                &(target, size),
                |bench, &(target, size)| {
                    bench.iter(|| micro::alloc_zeroed(target, size, SMALL_OPS));
                },
            );
        }
    }

    group.finish();
}
