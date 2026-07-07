use criterion::{Criterion, criterion_group, criterion_main};

mod common;

#[global_allocator]
static ALLOC: runic::RunicAlloc = runic::RunicAlloc::new();

fn global_collections(c: &mut Criterion) {
    common::register_global_collections(c, "runic");
}

criterion_group!(global_runic, global_collections);
criterion_main!(global_runic);
