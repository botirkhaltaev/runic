use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn workloads(c: &mut Criterion) {
    suite::workloads::register(c, "jemalloc");
}

criterion_group! {
    name = global_jemalloc;
    config = suite::criterion();
    targets = workloads
}
criterion_main!(global_jemalloc);
