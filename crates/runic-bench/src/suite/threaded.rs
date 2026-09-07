use std::{hint::black_box, time::Duration, time::Instant};

use criterion::{BenchmarkId, Criterion, Throughput};

use crate::{suite::elems, target::AllocatorTarget, threaded};

const THREAD_COUNTS: &[usize] = &[2, 4];
const LIFECYCLE_OPS: usize = 512;
const PERSISTENT_OPS: usize = 2_048;
const LIVE_DEPTHS: &[usize] = &[1, 32, 256];

pub fn register(c: &mut Criterion, targets: &[AllocatorTarget]) {
    register_lifecycle(c, targets);
    register_local_churn(c, targets);
    register_free_ring(c, targets);
    register_remote_fan_in(c, targets);
    register_owner_concurrent(c, targets);
    register_remote_reuse(c, targets);
    register_bound_remote(c, targets);
    register_unbound_remote(c, targets);
    register_owner_accept(c, targets);
}

fn register_lifecycle(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/lifecycle");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(elems(threads * LIFECYCLE_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter(|| threaded::lifecycle(target, threads, LIFECYCLE_OPS));
                },
            );
        }
    }

    group.finish();
}

fn register_local_churn(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/local_churn");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(elems(threads * PERSISTENT_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::LocalChurn::spawn(target, threads);
                        let start = Instant::now();
                        for _ in 0..iters {
                            black_box(workers.run_round(PERSISTENT_OPS));
                        }
                        let elapsed = start.elapsed();
                        drop(workers);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_free_ring(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/free_ring");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                group.throughput(Throughput::Elements(elems(threads * PERSISTENT_OPS)));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers = threaded::FreeRing::spawn(target, threads);
                            let start = Instant::now();
                            for _ in 0..iters {
                                black_box(workers.run_round(PERSISTENT_OPS, live));
                            }
                            let elapsed = start.elapsed();
                            drop(workers);
                            elapsed
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_remote_fan_in(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/remote_fan_in");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                group.throughput(Throughput::Elements(elems(threads * PERSISTENT_OPS)));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers = threaded::RemoteFanIn::spawn(target, threads);
                            let start = Instant::now();
                            for _ in 0..iters {
                                black_box(workers.run_round(PERSISTENT_OPS, live));
                            }
                            let elapsed = start.elapsed();
                            drop(workers);
                            elapsed
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_owner_concurrent(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/owner_concurrent");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                group.throughput(Throughput::Elements(elems(
                    PERSISTENT_OPS + threads * PERSISTENT_OPS,
                )));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers = threaded::OwnerConcurrent::spawn(target, threads);
                            let start = Instant::now();
                            for _ in 0..iters {
                                black_box(workers.run_round(PERSISTENT_OPS, live));
                            }
                            let elapsed = start.elapsed();
                            drop(workers);
                            elapsed
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn register_remote_reuse(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/remote_reuse");

    for &target in targets {
        for &live in LIVE_DEPTHS {
            group.throughput(Throughput::Elements(elems(PERSISTENT_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), format!("live:{live}")),
                &(target, live),
                |bench, &(target, live)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::RemoteReuse::spawn(target);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.run_round(PERSISTENT_OPS, live));
                            if let Some(ns) = workers.last_round_reuse_ns() {
                                elapsed += Duration::from_nanos(ns);
                            }
                        }
                        if let Some(mean_ns) = workers.mean_reuse_ns() {
                            eprintln!("runic_mean_reuse_ns={mean_ns}");
                        }
                        drop(workers);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_bound_remote(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/bound_remote");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(elems(threads * PERSISTENT_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::RemoteFree::spawn_bound(target, threads);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            let start = Instant::now();
                            black_box(workers.run_free_round(PERSISTENT_OPS));
                            elapsed += start.elapsed();
                        }
                        drop(workers);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_unbound_remote(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/unbound_remote");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(elems(threads * PERSISTENT_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::RemoteFree::spawn_unbound(target, threads);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            let start = Instant::now();
                            black_box(workers.run_free_round(PERSISTENT_OPS));
                            elapsed += start.elapsed();
                        }
                        drop(workers);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn register_owner_accept(c: &mut Criterion, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group("threaded/owner_accept");

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(elems(PERSISTENT_OPS)));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::RemoteFree::spawn_bound(target, threads);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            black_box(workers.run_free_round(PERSISTENT_OPS));
                            let start = Instant::now();
                            black_box(workers.run_accept_round(PERSISTENT_OPS));
                            elapsed += start.elapsed();
                        }
                        drop(workers);
                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}
