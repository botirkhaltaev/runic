# Runic Benchmarks

Allocator benchmark suite.

Each `#[global_allocator]` binary runs the same deterministic application
workloads. Every workload lives in one file under `src/workloads/` and owns
its input, implementation, checksum, and throughput count.

```sh
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench global_runic -- 'global/runic/json_api' --exact
```

- `global_{runic,system,mimalloc,jemalloc,snmalloc}`: process-global allocator.
- Collections: word count, growth-heavy `Vec` / `HashMap` / `VecDeque`, text
  indexing, LRU, record sorting, and graph search.
- Libraries: JSON, regex, HTTP parsing, CSV aggregation, gzip roundtrip, and
  TOML.
- Threaded: Tokio request handling, a long-lived worker pool, and a log
  pipeline. Profile these with `RUNIC_PROFILE_CPUS=0-3`.

Default Criterion settings are developer-sized (`sample_size=10`, 1s, 2000 resamples, no plots, no Rayon). CLI flags (`--measurement-time`, `--sample-size`, `--profile-time`) override them. Rayon is off because Criterion analysis allocates through `#[global_allocator]`.

## Profiling

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline global_runic 'global/runic/json_api'
scripts/profile.sh --compare target/runic-profiles/run-before target/runic-profiles/run-after
```

See `src/README.md` and `benches/README.md`.
