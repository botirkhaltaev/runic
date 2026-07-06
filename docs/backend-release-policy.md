# Backend Release Policy

Issue: #26

Runic's first backend release policy is intentionally small and deterministic. It applies to cached dedicated extent mappings, where reuse improves large-allocation churn but unbounded retention would inflate RSS.

## Policy

`MappingCache` owns the policy because it owns retained extent mappings:

- admit a freed mapping only if slot capacity and a hard byte budget allow it
- retain exact-size mappings for fast reuse
- decay retained mappings to a soft byte target after insertion
- release the oldest cached mappings first
- retain at least one mapping that is larger than the soft target when it fits under the hard cap

The policy is fixed for now. It is not adaptive and does not allocate internally.

## Current Budgets

- soft retained target: 8 MiB
- hard retained limit: 16 MiB
- slot limit: 32 mappings

These values keep the existing large-churn fast path useful while bounding mixed-size extent retention.

## Testing

The release path is deterministic enough to test in `MappingCache`:

- exact-size reuse still works
- slot and byte caps are enforced
- decay releases the oldest mapping when retained bytes exceed the soft target

## Measurement

Use Criterion for latency-sensitive paths:

- `explicit/large_alloc_churn/runic/32769`
- `explicit/large_alloc_churn/runic/65536`
- `explicit/large_alloc_churn/runic/262144`
- `explicit/large_alloc_churn/runic/1048576`

Use the RSS tool for resident-set behavior:

- `cargo run -p runic-bench --release --bin rss -- --case runic large_alloc_churn_256k`

Future mixed-large RSS cases should cover varied extent sizes before adding adaptive decay.

## Future Work

Thread-local heaps and hugepage-aware mappings may need separate policies. They should not reuse this cache as a passive wrapper; the backend entity that owns mmap/munmap lifecycle should own any additional retention, purge, or hugepage decisions.
