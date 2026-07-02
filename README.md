# Ferralloc

Ferralloc is an experimental Rust-native allocator project.

The first milestone is deliberately small: build a global-lock allocator core with mmap-backed spans, size classes, out-of-line metadata, pointer-to-span lookup, block boundary checks, basic `realloc`, basic `alloc_zeroed`, and randomized tests.

Correctness comes before speed.

## Workspace

```text
crates/ferralloc-core          allocator mechanics
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

See `docs/ferralloc-vision.md` and `docs/v0.1-plan.md` for the project thesis and implementation plan.
