# runic-bench/benches/common

Shared helpers for Criterion benchmark targets.

- `configure.rs`: shared Criterion group defaults.
- `mod.rs`: global-collection registration for `global_*` benches.
- `explicit.rs` / `threaded.rs`: ordinary Criterion registration (no profiling hooks).

Profiling is owned by `scripts/profile.sh` (resolved ELF under perf / samply / flamegraph).
Cost → metrics/`--compare`; Where → `perf report` / annotate on `perf.data`.
