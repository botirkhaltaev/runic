# runic-bench/src

Shared benchmark machinery for Criterion benchmarks and RSS reports.

## Modules

- `allocation`: allocation records and low-level allocation operations used by workloads.
- `allocator_target`: allocator selection across Runic, system, mimalloc, jemalloc, and snmalloc.
- `global_workload`: workloads that exercise a process-global allocator through ordinary Rust allocations.
- `measurement`: measurement helpers for benchmark runs.
- `report`: reporting utilities for benchmark output.
- `rng`: deterministic random number generation.
- `rss`: resident-set-size subprocess runner support.
- `threaded`: threaded workload definitions.
- `workload`: common workload shapes and validation.

Benchmark entry points live in `../benches/`; RSS and policy binaries live in
`bin/`. `policy_grid` is extent-policy-only; small and threaded frontend work
belongs in the Criterion benchmark targets.
