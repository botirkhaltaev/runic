use criterion::{Criterion, Throughput};

use crate::{collections, suite::elems};

pub fn register(c: &mut Criterion, allocator: &str) {
    let mut group = c.benchmark_group(format!("global/{allocator}"));
    group.throughput(Throughput::Elements(elems(1_024)));

    group.bench_function("vec_push_clear", |bench| {
        bench.iter(|| collections::vec_push_clear(32, 1_024));
    });
    group.bench_function("vec_many_small", |bench| {
        bench.iter(|| collections::vec_many_small(16, 1_024));
    });
    group.bench_function("string_building", |bench| {
        bench.iter(|| collections::string_building(32, 1_024));
    });
    group.bench_function("hashmap_insert_remove", |bench| {
        bench.iter(|| collections::hashmap_insert_remove(16, 1_024));
    });
    group.bench_function("arc_clone_drop", |bench| {
        bench.iter(|| collections::arc_clone_drop(32, 1_024));
    });
    group.bench_function("mixed_collections", |bench| {
        bench.iter(|| collections::mixed_collections(8, 1_024));
    });
    group.bench_function("tree", |bench| {
        bench.iter(|| collections::tree(8, 512));
    });
    group.bench_function("word_count", |bench| {
        bench.iter(|| collections::word_count(4, 4_096));
    });
    group.finish();
}
