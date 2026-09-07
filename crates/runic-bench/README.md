# Runic Benchmarks

Internal allocator benchsuite. Not published.

## Criterion suites

```sh
cargo bench -p runic-bench --bench micro
cargo bench -p runic-bench --bench threaded
cargo bench -p runic-bench --bench programs
cargo bench -p runic-bench --bench global_runic
```

Filter by allocator and case (every suite includes all allocators except `global_*`):

```sh
cargo bench -p runic-bench --bench micro -- 'micro/owner_free/runic/64' --exact
```

- `micro`: owner-local phase isolation and size-class matrix through `GlobalAlloc`.
- `threaded`: persistent workers plus one `lifecycle` bind/unbind path.
- `programs`: real-world-shaped workloads (larson, xmalloc, cache, sh6bench, cfrac).
- `global_*`: process-global allocator, std collections (`Vec`, `String`, `HashMap`, tree, word-count).

Default Criterion settings are developer-sized (`sample_size=10`, 1s, 2000 resamples, no plots, no Rayon). CLI flags (`--measurement-time`, `--sample-size`, `--profile-time`) override them. Rayon is off because Criterion analysis allocates through `#[global_allocator]`.

## Metrics (RSS / VMA / faults)

```sh
cargo run -p runic-bench --release --bin metrics
cargo run -p runic-bench --release --bin metrics -- --cases sh6bench --targets runic,mimalloc
cargo run -p runic-bench --release --bin metrics -- --syscalls --cases larson --targets runic
```

Each allocator/case pair runs in a fresh subprocess so `VmHWM` is per case. Columns: peak RSS, plateau RSS after free, VMA count, minor faults, optional `mmap`/`madvise` syscall counts.

Runic extent configs: `--targets runic:extent_drop,runic:extent_tight`.

## Validation

Workloads touch allocated memory, check alignment, and sample realloc prefix markers. Full correctness stays in the allocator test suite.

See `src/README.md` and `benches/README.md`.
