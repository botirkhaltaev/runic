# Runic

Runic is a correctness-first Rust allocator with a small auditable unsafe core, out-of-line metadata, and explicit allocation invariants.

The current release is an experimental v0.4 global-lock allocator for Linux x86_64. It is useful for allocator development, single-thread performance work, retention-policy experiments, tests, and architecture iteration; it is not yet a production allocator.

## Install

The public allocator crate is published as `runic-alloc`:

```sh
cargo add runic-alloc
```

The Rust library name is `runic`, so code imports `runic::RunicAlloc`.

## Usage

Use `RunicAlloc` as a Rust global allocator:

```rust
use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();

fn main() {
    let values = vec![1, 2, 3, 4];
    assert_eq!(values.len(), 4);
}
```

## Status

Runic v0.4 implements:

- `GlobalAlloc`
- one global heap lock
- mmap-backed runs for small size classes
- mmap-backed extents for dedicated allocations
- out-of-line metadata
- page-indexed owner-pointer lookup
- per-size-class available run lists
- per-block AtomicU8 run block state (reusable / allocated / remote-pending)
- configurable extent mapping retention and reuse policies
- runs retained for the heap lifetime (no empty-run OS release in v0.5)
- run block-boundary checks
- extent exact-pointer checks
- basic `realloc`
- basic `alloc_zeroed`
- randomized allocation trace tests

Correctness comes before speed. See `ROADMAP.md` for the project thesis, current scope, architecture, benchmarks, and follow-up plan.

## Crates

```text
crates/runic-core          allocator mechanics and global state; published as runic-core
crates/runic               GlobalAlloc wrapper; published as runic-alloc, imported as runic
crates/runic-test-support  reusable test machinery; not published
crates/runic-bench         benchmark harness; not published
```

Published crates:

- `runic-alloc`: https://crates.io/crates/runic-alloc
- `runic-core`: https://crates.io/crates/runic-core

## Development

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo bench -p runic-bench --no-run
```

Use `perf stat` for same-machine benchmark comparisons. For example:

```sh
cargo bench -p runic-bench --bench explicit --no-run
perf stat -r 3 -e task-clock,cycles,instructions,branches,branch-misses,cache-misses \
  ./target/release/deps/explicit-* explicit/alloc_zeroed/runic/4096 --bench
```

## Release

Release tags use plain semver, for example `0.4.0`.

Release `runic-core` before `runic-alloc`, because `runic-alloc` depends on the published `runic-core` version during package verification.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
