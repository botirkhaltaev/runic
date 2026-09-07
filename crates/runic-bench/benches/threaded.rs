use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::{suite, target::TARGETS};

fn threaded(c: &mut Criterion) {
    suite::threaded::register(c, TARGETS);
}

criterion_group! {
    name = threaded_benches;
    config = suite::criterion();
    targets = threaded
}
criterion_main!(threaded_benches);
