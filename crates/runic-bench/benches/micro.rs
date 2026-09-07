use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::{suite, target::TARGETS};

fn micro(c: &mut Criterion) {
    suite::micro::register(c, TARGETS);
}

criterion_group! {
    name = micro_benches;
    config = suite::criterion();
    targets = micro
}
criterion_main!(micro_benches);
