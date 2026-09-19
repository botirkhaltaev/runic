# runic-bench/src

Shared machinery for Criterion suites.

## Modules

- `rng`: deterministic `TraceRng`.
- `workloads`: one self-contained file per application workload plus a small
  registry containing only name, throughput elements, and run function.
- `suite`: Criterion registration. `criterion()` sets defaults (no plots, 2000 resamples); CLI overrides via `configure_from_args`. Criterion is built without Rayon so `global_*` analysis cannot exhaust Runic's 64 heaps.
