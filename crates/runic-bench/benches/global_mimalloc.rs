use criterion::{Criterion, criterion_group, criterion_main};
use runic_bench::suite;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn collections(c: &mut Criterion) {
    suite::collections::register(c, "mimalloc");
}

criterion_group! {
    name = global_mimalloc;
    config = suite::criterion();
    targets = collections
}
criterion_main!(global_mimalloc);
