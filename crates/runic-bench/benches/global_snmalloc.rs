use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

fn collections(c: &mut Criterion) {
    suite::collections::register(c, "snmalloc");
}

criterion_group! {
    name = global_snmalloc;
    config = suite::criterion();
    targets = collections
}
criterion_main!(global_snmalloc);
