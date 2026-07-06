# Runic

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/botirkhaltaev/runic?utm_source=badge)

Runic is a correctness-first Rust allocator with a small auditable unsafe core, out-of-line metadata, and explicit allocation invariants.

The current release is an experimental v0.1 global-lock allocator for Linux x86_64. It is useful for allocator development, tests, and architecture work; it is not yet tuned for production performance.

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
static GLOBAL: RunicAlloc = RunicAlloc;

fn main() {
    let values = vec![1, 2, 3, 4];
    assert_eq!(values.len(), 4);
}
```

## Status

Runic v0.1 implements:

- `GlobalAlloc`
- one global heap lock
- mmap-backed runs for small size classes
- mmap-backed extents for dedicated allocations
- out-of-line metadata
- page-indexed pointer lookup
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

## Release

Release tags use plain semver, for example `0.1.0`.

Pushing a release tag runs CI, validates package versions, publishes `runic-core` and `runic-alloc` to crates.io, and creates a GitHub Release.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
