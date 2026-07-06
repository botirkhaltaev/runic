# runic-bench/benches

Criterion benchmark entry points.

## Runic Targets

- `explicit`: Runic-only direct `GlobalAlloc` workloads.
- `threaded`: Runic-only threaded workloads.
- `global_runic`: process-global Runic allocator workloads.

Use these targets for same-machine `perf stat` comparisons of Runic changes without noise from external allocator comparison runs.

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

## Perf

Use `perf stat` against exact benchmark binaries when checking regressions:

```sh
cargo bench -p runic-bench --bench explicit --no-run
perf stat -r 3 -e task-clock,cycles,instructions,branches,branch-misses,cache-misses \
  ./target/release/deps/explicit-* explicit/alloc_zeroed/runic/4096 --bench
```

Compare base and head on the same machine, preferably from separate git worktrees built from exact commits.
