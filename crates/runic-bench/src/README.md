# runic-bench/src

Shared machinery for Criterion suites and the `metrics` binary.

## Modules

- `target`: allocator names (`runic`, `system`, `mimalloc`, `jemalloc`, `snmalloc`).
- `rng`: deterministic `TraceRng`.
- `collections`: process-global `Vec` / `String` / `HashMap` / tree / word-count.
- `libraries`: `serde_json` API traffic, `regex` log scan, `bytes` buffers, large read/decode buffers.
- `threaded`: 4-thread channel pipeline, Arc last-drop, scoped map-reduce.
- `metrics`: RSS peak/plateau, VMA count, minor faults, CSV.
- `suite`: Criterion registration. `criterion()` sets defaults (no plots, 2000 resamples); CLI overrides via `configure_from_args`. Criterion is built without Rayon so `global_*` analysis cannot exhaust Runic's 64 heaps.
