use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn collections(c: &mut Criterion) {
    suite::collections::register(c, "jemalloc");
}

criterion_group! {
    name = global_jemalloc;
    config = suite::criterion();
    targets = collections
}
criterion_main!(global_jemalloc);
