use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::allocator_target::RUNIC_TARGETS;

#[path = "common/threaded.rs"]
mod threaded_common;

fn threaded(c: &mut Criterion) {
    threaded_common::register(c, "threaded", RUNIC_TARGETS);
}

criterion_group!(threaded_benches, threaded);
criterion_main!(threaded_benches);
