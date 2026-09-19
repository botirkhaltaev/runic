use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

fn workloads(c: &mut Criterion) {
    suite::workloads::register(c, "system");
}

criterion_group! {
    name = global_system;
    config = suite::criterion();
    targets = workloads
}
criterion_main!(global_system);
