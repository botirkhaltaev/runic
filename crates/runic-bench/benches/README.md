# runic-bench/benches

Criterion benchmark entry points.

## Runic Targets

- `explicit`: Runic-only direct `GlobalAlloc` workloads.
- `threaded`: Runic-only threaded workloads (setup-lifecycle + persistent).
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

## Threaded filters

Setup/lifecycle (spawn/join inside the timed region — not for hot-path profiles):

- `threaded/setup_lifecycle_thread_local_churn/runic/4`
- `threaded/setup_lifecycle_cross_thread_free_ring/runic/4`
- `threaded/setup_lifecycle_draining_late_free/runic/4`

Persistent workers (preferred for allocator profiles):

- `threaded/persistent_local_churn/runic/4`
- `threaded/persistent_cross_thread_ring/runic/4/live:256`
- `threaded/persistent_remote_fan_in/runic/4/live:256`
- `threaded/persistent_owner_concurrent/runic/4/live:256`
- `threaded/persistent_remote_reuse_latency/runic/live:1`

## Run

```sh
cargo bench -p runic-bench
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench compare_explicit
```

## Perf

Preferred: `scripts/profile.sh` (builds once, runs the resolved bench binary under perf):

```sh
scripts/profile.sh -l baseline explicit 'explicit/single_size_churn/runic/64'
scripts/profile.sh -l baseline explicit 'explicit/recycled_live_churn/runic/64/live:1'
scripts/profile.sh -l baseline -t 20 \
  threaded 'threaded/persistent_remote_fan_in/runic/4/live:256'
scripts/profile.sh -t 20 -a 'runic_core::heap::run::Run::free' \
  explicit 'explicit/alloc_zeroed/runic/64'
```

Recycled live-set matrix (all 27 size classes × depths 1 / 32 / 256):

- `explicit/recycled_live_churn/runic/{size}/live:{depth}`

Artifacts land under `target/runic-profiles/` (override with `RUNIC_PROFILE_DIR` or `-o`).
Each run writes `manifest.txt`, `metrics.txt`, and `summary.txt`.

Compare base and head on the same machine, preferably from separate git worktrees built from exact commits.
