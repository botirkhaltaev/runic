use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::{suite, target::TARGETS};

fn programs(c: &mut Criterion) {
    suite::programs::register(c, TARGETS);
}

criterion_group! {
    name = programs_benches;
    config = suite::criterion();
    targets = programs
}
criterion_main!(programs_benches);
