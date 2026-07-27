use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput};
use runic_bench::{allocator_target::AllocatorTarget, threaded};

const THREAD_COUNTS: &[usize] = &[2, 4];
const OPS_PER_THREAD: usize = 512;
const PERSISTENT_OPS: usize = 2_048;
const LIVE_DEPTHS: &[usize] = &[1, 32, 256];

fn configure_group(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1));
}

pub fn register(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    register_setup_lifecycle_thread_local_churn(c, suite, targets);
    register_setup_lifecycle_cross_thread_free_ring(c, suite, targets);
    register_setup_lifecycle_mixed_thread_random(c, suite, targets);
    register_setup_lifecycle_draining_late_free(c, suite, targets);

    register_persistent_local_churn(c, suite, targets);
    register_persistent_cross_thread_ring(c, suite, targets);
    register_persistent_remote_fan_in(c, suite, targets);
    register_persistent_owner_concurrent(c, suite, targets);
    register_persistent_remote_reuse_latency(c, suite, targets);
    register_persistent_bound_remote_batch(c, suite, targets);
    register_persistent_unbound_remote_singleton(c, suite, targets);
    register_persistent_owner_accept(c, suite, targets);
}

fn register_setup_lifecycle_thread_local_churn(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/setup_lifecycle_thread_local_churn"));
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

fn register_setup_lifecycle_cross_thread_free_ring(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/setup_lifecycle_cross_thread_free_ring"));
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

fn register_setup_lifecycle_mixed_thread_random(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/setup_lifecycle_mixed_thread_random"));
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

fn register_setup_lifecycle_draining_late_free(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/setup_lifecycle_draining_late_free"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter(|| threaded::draining_late_free(target, threads, OPS_PER_THREAD));
                },
            );
        }
    }

    group.finish();
}

fn register_persistent_local_churn(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_local_churn"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * PERSISTENT_OPS) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::PersistentLocalChurn::spawn(target, threads);
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

fn register_persistent_cross_thread_ring(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_cross_thread_ring"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                group.throughput(Throughput::Elements((threads * PERSISTENT_OPS) as u64));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers =
                                threaded::PersistentCrossThreadRing::spawn(target, threads);
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

fn register_persistent_remote_fan_in(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_remote_fan_in"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                group.throughput(Throughput::Elements((threads * PERSISTENT_OPS) as u64));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers = threaded::PersistentRemoteFanIn::spawn(target, threads);
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

fn register_persistent_owner_concurrent(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_owner_concurrent"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            for &live in LIVE_DEPTHS {
                // Elements: owner local churn steps + remote frees (ops * freers).
                group.throughput(Throughput::Elements(
                    (PERSISTENT_OPS + threads * PERSISTENT_OPS) as u64,
                ));
                group.bench_with_input(
                    BenchmarkId::new(target.name(), format!("{threads}/live:{live}")),
                    &(target, threads, live),
                    |bench, &(target, threads, live)| {
                        bench.iter_custom(|iters| {
                            let workers =
                                threaded::PersistentOwnerConcurrent::spawn(target, threads);
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

fn register_persistent_remote_reuse_latency(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_remote_reuse_latency"));
    configure_group(&mut group);

    for &target in targets {
        for &live in LIVE_DEPTHS {
            group.throughput(Throughput::Elements(PERSISTENT_OPS as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), format!("live:{live}")),
                &(target, live),
                |bench, &(target, live)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::PersistentRemoteReuse::spawn(target);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.run_round(PERSISTENT_OPS, live));
                            if let Some(ns) = workers.last_round_reuse_ns() {
                                elapsed += Duration::from_nanos(ns);
                            }
                        }
                        if let Some(mean_ns) = workers.mean_reuse_ns() {
                            // Parseable marker for scripts/profile.sh → metrics.txt.
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

fn register_persistent_bound_remote_batch(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_bound_remote_batch"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * PERSISTENT_OPS) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers =
                            threaded::PersistentBoundRemoteBatch::spawn_bound(target, threads);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            let start = Instant::now();
                            black_box(workers.run_free_round());
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

fn register_persistent_unbound_remote_singleton(
    c: &mut Criterion,
    suite: &str,
    targets: &[AllocatorTarget],
) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_unbound_remote_singleton"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements((threads * PERSISTENT_OPS) as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers = threaded::PersistentUnboundRemoteSingleton::spawn_unbound(
                            target, threads,
                        );
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            let start = Instant::now();
                            black_box(workers.run_free_round());
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

fn register_persistent_owner_accept(c: &mut Criterion, suite: &str, targets: &[AllocatorTarget]) {
    let mut group = c.benchmark_group(format!("{suite}/persistent_owner_accept"));
    configure_group(&mut group);

    for &target in targets {
        for &threads in THREAD_COUNTS {
            group.throughput(Throughput::Elements(PERSISTENT_OPS as u64));
            group.bench_with_input(
                BenchmarkId::new(target.name(), threads),
                &(target, threads),
                |bench, &(target, threads)| {
                    bench.iter_custom(|iters| {
                        let workers =
                            threaded::PersistentBoundRemoteBatch::spawn_bound(target, threads);
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            black_box(workers.prepare_round(PERSISTENT_OPS));
                            black_box(workers.run_free_round());
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
