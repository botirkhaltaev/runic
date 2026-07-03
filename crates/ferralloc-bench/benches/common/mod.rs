use std::time::Duration;

use criterion::{Criterion, Throughput};
use ferralloc_bench::global_workload;

pub fn register_global_collections(c: &mut Criterion, allocator: &str) {
    let mut group = c.benchmark_group(format!("global/{allocator}/collections"));
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1))
        .throughput(Throughput::Elements(1_024));

    group.bench_function("vec_push_clear", |bench| {
        bench.iter(|| global_workload::vec_push_clear(32, 1_024));
    });
    group.bench_function("vec_many_small", |bench| {
        bench.iter(|| global_workload::vec_many_small(16, 1_024));
    });
    group.bench_function("string_building", |bench| {
        bench.iter(|| global_workload::string_building(32, 1_024));
    });
    group.bench_function("hashmap_insert_remove", |bench| {
        bench.iter(|| global_workload::hashmap_insert_remove(16, 1_024));
    });
    group.bench_function("arc_clone_drop", |bench| {
        bench.iter(|| global_workload::arc_clone_drop(32, 1_024));
    });
    group.bench_function("mixed_collections", |bench| {
        bench.iter(|| global_workload::mixed_collections(8, 1_024));
    });
    group.finish();
}
