use criterion::{Criterion, Throughput};

use crate::{suite::elems, workloads::WORKLOADS};

pub fn register(c: &mut Criterion, allocator: &str) {
    let mut group = c.benchmark_group(format!("global/{allocator}"));

    for workload in WORKLOADS {
        group.throughput(Throughput::Elements(elems(workload.elems())));
        group.bench_function(workload.name(), |bench| {
            bench.iter(|| workload.run());
        });
    }
    group.finish();
}
