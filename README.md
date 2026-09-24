# Runic

Runic is an owner-local allocator for Linux `x86_64`. Metadata sits outside
user allocations. Unsafe code is limited to OS and ownership boundaries.
Invalid and foreign frees abort. Owner double-free is undefined. C
`free(NULL)` is a no-op.

Version **0.8.0** provides `GlobalAlloc` and the C malloc family for
`LD_PRELOAD`. It requires nightly Rust (`#[thread_local]`). Hardening,
hugepages, NUMA, and background purge are not implemented. See
[Compatibility](COMPATIBILITY.md). Planned work is the
[Roadmap](ROADMAP.md).

## Rust

```sh
cargo add runic-alloc
```

The package is `runic-alloc`; import `runic::RunicAlloc`.

```rust
use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();
```

[runic-alloc API](https://docs.rs/runic-alloc)

## C / LD_PRELOAD

```sh
cargo build -p runic-cabi --release
LD_PRELOAD=$PWD/target/release/librunic.so ./program
```

[C ABI contract](crates/runic-cabi/README.md)

## Read next

| Document | Contents |
|----------|----------|
| [Architecture](ARCHITECTURE.md) | Flows, ownership, heap lifecycle, remote free |
| [Compatibility](COMPATIBILITY.md) | Supported APIs, platforms, and gaps |
| [Roadmap](ROADMAP.md) | Thesis, releases, next work |
| [Measurement diary](diary.md) | Experiment results |

## Develop

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo bench -p runic-bench --no-run
```

`cargo test --workspace` builds `librunic.so` and runs the preload tests.

## License

Apache-2.0 or MIT.
