# runic-core

`runic-core` contains Runic's allocator mechanics and global allocator state.

This crate is published for the public `runic-alloc` crate, but most modules are internal. The main public entry point is `runic_core::Allocator`. It requires Rust nightly (`#![feature(thread_local)]`).

## Responsibilities

- Normalize allocation layouts.
- Select size classes.
- Manage heap-owned run maps and dedicated extents.
- Store run headers in the run space (`base + RUN_SIZE`) and extent metadata in an arena.
- Map returned pointers back to borrowed run/extent owners.
- Keep raw pointer decoding and intrusive traversal inside page-map, run-heap, and inbox leaves.
- Enforce run block-boundary checks and extent exact-pointer checks.

## Usage

Most users should depend on `runic-alloc`, not `runic-core` directly.

```toml
[dependencies]
runic-alloc = "0.6.0"
```

## Development

```sh
cargo test -p runic-core
cargo clippy -p runic-core --all-targets --all-features -- -D warnings
```

See `src/README.md` for module responsibilities.
