# runic-bench/src

Shared machinery for Criterion suites and the `metrics` binary.

## Modules

- `target`: `AllocatorTarget` and `TARGETS` (runic, system, mimalloc, jemalloc, snmalloc).
- `record`: allocation records with marker validation.
- `rng`: deterministic `TraceRng`.
- `micro`: owner-local phase workloads and `Live` guard.
- `threaded`: `Workers` pool and persistent / lifecycle workloads.
- `programs`: larson, xmalloc, cache_thrash/scratch, sh6bench, cfrac.
- `collections`: process-global `Vec` / `String` / `HashMap` / tree / word-count.
- `metrics`: RSS peak/plateau, VMA count, minor faults, CSV.
- `suite`: Criterion registration. `criterion()` sets defaults (no plots, 2000 resamples); CLI overrides via `configure_from_args`. Criterion is built without Rayon so `global_*` analysis cannot exhaust Runic's 64 heaps.

## Threaded

- Persistent groups spawn `Workers` once per Criterion sample (`iter_custom`) and time only `run_round`.
- `lifecycle` times spawn + one churn round + join (bind/unbind/drain).
- `bound_remote` / `unbound_remote` allocate in `prepare_round` and time only freer drains.
- `owner_accept` prepares and frees outside timing; measures owner accept/flush only.
- `remote_reuse` Criterion duration is measured reuse latency; emits `runic_mean_reuse_ns=`.

## Phase-isolated local

- `owner_free` / `freelist_allocate` use `iter_batched` so fill/seed/drop sit outside the timed window (`LOCAL_PHASE_SIZES`: 8 / 64 / 80 / 4096).
