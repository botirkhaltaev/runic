use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::allocator_target::TARGETS;

#[path = "common/threaded.rs"]
mod threaded_common;

fn compare_threaded(c: &mut Criterion) {
    threaded_common::register(c, "compare/threaded", TARGETS);
}

criterion_group!(compare_threaded_benches, compare_threaded);
criterion_main!(compare_threaded_benches);
