# Ferralloc

Ferralloc is an experimental Rust-native allocator project.

The first milestone is deliberately small: build a global-lock allocator core with size-classed runs, dedicated extents, out-of-line metadata, page-indexed pointer lookup, block-boundary checks, exact-pointer extent frees, basic `realloc`, basic `alloc_zeroed`, and randomized tests.

Correctness comes before speed.

## Workspace

```text
crates/ferralloc-core          allocator mechanics and global state
crates/ferralloc               GlobalAlloc wrapper
crates/ferralloc-test-support  reusable test machinery
crates/ferralloc-bench         benchmark harness
```

## Commands

```sh
cargo check --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

See `ROADMAP.md` for the project thesis, current scope, architecture, and follow-up plan.
