# runic-bench/src

Shared benchmark machinery for Criterion benchmarks and RSS reports.

## Modules

- `allocation`: allocation records and low-level allocation operations used by workloads.
- `allocator_target`: allocator selection across Runic, system, mimalloc, jemalloc, and snmalloc.
- `global_workload`: workloads that exercise a process-global allocator through ordinary Rust allocations.
- `rng`: deterministic random number generation.
- `rss`: resident-set-size subprocess runner support.
- `threaded`: threaded and persistent-worker workload definitions.
- `workload`: common workload shapes and validation.

Benchmark entry points live in `../benches/`; RSS and policy binaries live in
`bin/`. `policy_grid` is extent-policy-only; small and threaded frontend work
belongs in the Criterion benchmark targets.

## Threaded workloads

- `setup_lifecycle_*` Criterion groups call spawn/join inside each iteration — use for lifecycle noise, not allocator hot-path profiles.
- `persistent_*` groups spawn workers once per Criterion sample via `iter_custom` and time only `run_round` — use with `scripts/profile.sh`.
- `persistent_bound_remote` / `persistent_unbound_remote` allocate outside the timed region (`prepare_round`) and time only channel-free freer drains (`run_free_round`).
- `persistent_owner_accept` runs prepare + free outside timing and measures owner `run_accept_round` (flush/accept) only.
- `persistent_remote_fan_in` honors freer `live` backlog depth; `persistent_owner_concurrent` mixes owner-local churn with remote frees (not a fan-in alias).
- `persistent_remote_reuse_latency` varies freer backlog via `live:{1,32,256}`; Criterion duration is measured reuse latency and emits `runic_mean_reuse_ns=`.

## Phase-isolated local workloads

- `owner_free_only` / `freelist_allocate_only` in `workload` fill or seed outside the timed window (`LOCAL_PHASE_SIZES`: 8 / 64 / 80 / 4096).
