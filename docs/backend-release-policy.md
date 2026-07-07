# Backend Release Policy

Issue: #26

Runic's backend release policy should be small and deterministic. It applies to cached dedicated extent mappings, where reuse improves large-allocation churn but unbounded retention would inflate RSS.

The current implementation exposes deterministic extent and empty-run policies through configured allocator instances. Defaults preserve the v0.3 behavior while policy-grid benchmarks compare alternatives.

## Policy

`ExtentCache` should own the policy because it owns retained extent mappings:

- admit a freed mapping only if slot capacity and a hard byte budget allow it
- retain exact-size mappings for fast reuse
- optionally evict retained mappings by FIFO, LIFO, LRU, largest, or smallest policy
- decay retained mappings to a soft byte target only after benchmarks show no latency regression
- release selected cached mappings through a deterministic order, such as oldest first
- retain at least one mapping that is larger than the soft target when it fits under the hard cap

The current implementation has fixed-array slot storage and configured slot/byte caps. Any new decay behavior must be fixed, non-adaptive, and allocation-free until benchmark evidence justifies additional complexity.

## Current And Candidate Budgets

- current hard retained limit: 16 MiB
- current slot limit: 32 mappings
- candidate soft retained target: 8 MiB, only if benchmarked as non-regressing

The current hard cap keeps the existing large-churn fast path useful. A soft target should be added only with mixed-size benchmarks that show bounded RSS without slower churn.

## Testing

The current cache is deterministic enough to test in `ExtentCache`:

- exact-size reuse still works
- slot and byte caps are enforced
- configured eviction releases mappings in a deterministic order when retained bytes exceed the hard target

## Measurement

Use Criterion for latency-sensitive paths:

- `explicit/large_alloc_churn/runic/32769`
- `explicit/large_alloc_churn/runic/65536`
- `explicit/large_alloc_churn/runic/262144`
- `explicit/large_alloc_churn/runic/1048576`

Use the RSS tool for resident-set behavior:

- `cargo run -p runic-bench --release --bin rss -- --case runic large_alloc_churn_256k`

Future mixed-large RSS cases should cover varied extent sizes before adding adaptive decay.

Use the policy grid for configured allocator comparisons:

- `cargo run -p runic-bench --release --bin policy_grid`

## Future Work

Thread-local heaps and hugepage-aware mappings may need separate policies. They should not reuse this cache as a passive wrapper; the backend entity that owns mmap/munmap lifecycle should own any additional retention, purge, or hugepage decisions.

First policy-grid signal: extent retention preserves large-churn performance relative to dropping mappings; empty-run release policies regress single-block churn and should remain opt-in unless later workloads justify them.
