use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

fn workloads(c: &mut Criterion) {
    suite::workloads::register(c, "snmalloc");
}

criterion_group! {
    name = global_snmalloc;
    config = suite::criterion();
    targets = workloads
}
criterion_main!(global_snmalloc);
