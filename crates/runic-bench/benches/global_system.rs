use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

fn collections(c: &mut Criterion) {
    suite::collections::register(c, "system");
}

criterion_group! {
    name = global_system;
    config = suite::criterion();
    targets = collections
}
criterion_main!(global_system);
