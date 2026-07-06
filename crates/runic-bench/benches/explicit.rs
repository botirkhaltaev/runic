use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::allocator_target::RUNIC_TARGETS;

#[path = "common/explicit.rs"]
mod explicit_common;

fn explicit(c: &mut Criterion) {
    explicit_common::register(c, "explicit", RUNIC_TARGETS);
}

criterion_group!(explicit_benches, explicit);
criterion_main!(explicit_benches);
