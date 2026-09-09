# Runic Benchmarks

Internal allocator benchsuite. Not published.

## Criterion suites

One `#[global_allocator]` binary per allocator. Cases are std collections plus
real crate traffic (`serde_json`, `regex`, `bytes`).

```sh
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench global_runic -- 'global/runic/json_api' --exact
```

- `global_{runic,system,mimalloc,jemalloc,snmalloc}`: process-global allocator.
- Collections: `Vec`, `String`, `HashMap`, `Arc`, tree, word-count.
- Libraries: JSON API roundtrip, regex log scan, `bytes` HTTP buffers, large read/decode buffers.
- Threaded: channel pipeline, Arc last-drop, scoped map-reduce (profile with `RUNIC_PROFILE_CPUS=0-3`).

Default Criterion settings are developer-sized (`sample_size=10`, 1s, 2000 resamples, no plots, no Rayon). CLI flags (`--measurement-time`, `--sample-size`, `--profile-time`) override them. Rayon is off because Criterion analysis allocates through `#[global_allocator]`.

## Metrics (RSS / VMA / faults)

```sh
cargo run -p runic-bench --release --bin metrics
cargo run -p runic-bench --release --bin metrics -- --cases json_api,tree --targets runic,snmalloc
cargo run -p runic-bench --release --bin metrics -- --syscalls --cases http_buffers --targets runic
```

Each allocator/case pair runs in a fresh subprocess so `VmHWM` is per case. Columns: peak RSS, plateau RSS after free, VMA count, minor faults, optional `mmap`/`madvise` syscall counts.

Runic configs: `--targets runic:extent_unmap,runic:extent_tight,runic:run_discard,runic:extent_discard`.

See `src/README.md` and `benches/README.md`.
