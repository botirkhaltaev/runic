# runic-bench/benches

Criterion benchmark entry points.

## CodSpeed Targets

- `explicit`: Runic-only direct `GlobalAlloc` workloads.
- `threaded`: Runic-only threaded workloads.
- `global_runic`: process-global Runic allocator workloads.

CodSpeed runs only these targets so PR checks measure Runic changes, not noise from external allocator comparison runs.

## Manual Comparison Targets

- `compare_explicit`: direct `GlobalAlloc` workloads across Runic and external allocators.
- `compare_threaded`: threaded workloads across Runic and external allocators.
- `global_system`: process-global system allocator workloads.
- `global_mimalloc`: process-global mimalloc workloads.
- `global_jemalloc`: process-global jemalloc workloads.
- `global_snmalloc`: process-global snmalloc workloads.
- `common`: shared benchmark target setup.

## Run

```sh
cargo bench -p runic-bench
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench compare_explicit
```
