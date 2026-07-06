use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::allocator_target::TARGETS;

#[path = "common/explicit.rs"]
mod explicit_common;

fn compare_explicit(c: &mut Criterion) {
    explicit_common::register(c, "compare/explicit", TARGETS);
}

criterion_group!(compare_explicit_benches, compare_explicit);
criterion_main!(compare_explicit_benches);
