# runic-core

`runic-core` contains Runic's allocator mechanics and global allocator state.

This crate is published for the public `runic-alloc` crate, but most modules are internal. The main public entry point is `runic_core::Allocator`.

## Responsibilities

- Normalize allocation layouts.
- Select size classes.
- Manage mmap-backed runs and dedicated extents.
- Store out-of-line metadata in run and extent tables.
- Map returned pointers back to page-map owners.
- Enforce run block-boundary checks and extent exact-pointer checks.

## Usage

Most users should depend on `runic-alloc`, not `runic-core` directly.

```toml
[dependencies]
runic-alloc = "0.2.0"
```

## Development

```sh
cargo test -p runic-core
cargo clippy -p runic-core --all-targets --all-features -- -D warnings
```

See `src/README.md` for module responsibilities.
