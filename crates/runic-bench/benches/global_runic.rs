use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: runic::RunicAlloc = runic::RunicAlloc::new();

fn workloads(c: &mut Criterion) {
    suite::workloads::register(c, "runic");
}

criterion_group! {
    name = global_runic;
    config = suite::criterion();
    targets = workloads
}
criterion_main!(global_runic);
