# Runic benchmarks

Each `#[global_allocator]` binary runs the same deterministic workloads. Each
file under `src/workloads/` defines its input, implementation, checksum, and
throughput.

```sh
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench global_runic -- 'global/runic/json_api' --exact
```

Targets: `global_{runic,system,mimalloc,jemalloc,snmalloc}`. Names:
`global/<alloc>/<workload>`. Threaded workloads: `RUNIC_PROFILE_CPUS=0-3`.

Defaults are `sample_size=10`, 1 second, 2000 resamples, and no plots. CLI flags
override them. Criterion is built without Rayon so its analysis does not
allocate through the tested global allocator on every CPU.

Competitor crates (defaults, no extra features):

```text
snmalloc-rs 0.3.x   cmake Release -O3, initial-exec TLS; native-cpu off
mimalloc 0.1.x      v3, MI_SECURE off, initial-exec TLS
jemalloc 0.6.x      background_threads_runtime_support only
```

`[profile.bench]` is `lto = "fat"`, `codegen-units = 1`. Retain a change only
when this corpus improves and no workload regresses more than 1%.

## Profiling

`scripts/profile.sh` records counters in `metrics.txt`; `--compare` reports the
cost change. Use `perf report`, annotate, or samply on `perf.data` to locate it.

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline global_runic 'global/runic/json_api'
scripts/profile.sh --compare target/runic-profiles/run-before target/runic-profiles/run-after
```

See [src/README.md](src/README.md) for the registry,
[benches/README.md](benches/README.md) for targets, and
[diary.md](../../diary.md) for experiment results.
