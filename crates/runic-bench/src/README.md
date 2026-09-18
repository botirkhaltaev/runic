# runic-bench/src

Shared machinery for Criterion suites and the `metrics` binary.

## Modules

- `target`: allocator names (`runic`, `system`, `mimalloc`, `jemalloc`, `snmalloc`).
- `rng`: deterministic `TraceRng`.
- `collections`: process-global `Vec` / `String` / `HashMap` / tree / word-count / bursty run churn.
- `libraries`: `serde_json` API traffic, `regex` log scan, `bytes` buffers, large read/decode buffers (zeroed and dirty).
- `threaded`: 4-thread channel pipeline, Arc last-drop, scoped map-reduce, `spawn_churn`, `oversubscribed` (`4 * available_parallelism` threads).
- `workloads`: name / `elems` / `run` table shared by Criterion and `metrics`; `elems` is a function because `oversubscribed` scales with the host.
- `metrics`: RSS peak/plateau, VMA count, minor faults, CSV.
- `suite`: Criterion registration. `criterion()` sets defaults (no plots, 2000 resamples); CLI overrides via `configure_from_args`. Criterion is built without Rayon so `global_*` analysis cannot exhaust Runic's 64 heaps.
