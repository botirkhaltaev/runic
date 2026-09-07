use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: runic::RunicAlloc = runic::RunicAlloc::new();

fn collections(c: &mut Criterion) {
    suite::collections::register(c, "runic");
}

criterion_group! {
    name = global_runic;
    config = suite::criterion();
    targets = collections
}
criterion_main!(global_runic);
