# runic-alloc

`runic-alloc` is Runic's public `GlobalAlloc` wrapper crate. The `c-abi` feature exports the C malloc family from its cdylib for `LD_PRELOAD`.

The package name on crates.io is `runic-alloc`; the Rust library name is `runic`.

## Install

```sh
cargo add runic-alloc
```

## Usage

```rust
use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();
```

Use the const builder for explicit retention policy experiments. Extent policy
controls free-side retention: `Keep` retains a freed mapping while slot and byte
budgets allow it, `Discard` retains then `madvise`s the pages, `Unmap` does not
retain. Allocation-side lookup always reuses a retained mapping by exact length.
Slot and byte budgets are enforced exactly.

```rust
use runic::{Budget, ExtentPolicy, RunPolicy, RunicAlloc};

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Keep)
    .extent_budget(Budget::new(64, 64 * 1024 * 1024))
    .run_policy(RunPolicy::Keep)
    .build();
```

`dealloc` requires a live pointer this allocator returned. Null is forbidden (`GlobalAlloc` contract) and aborts; it is not a no-op.

## LD_PRELOAD (C / mimalloc-bench)

Build the interceptor shared object:

```sh
cargo build -p runic-alloc --release --features c-abi
```

That produces `target/release/librunic.so` with `malloc` / `free` / `calloc` / `realloc`, `posix_memalign` / `aligned_alloc` / `memalign`, `malloc_usable_size`, `valloc` / `reallocarray` / `cfree`, and glibc `__libc_*` aliases. `free(NULL)` is a no-op. Unknown pointers abort.

```sh
LD_PRELOAD=/path/to/target/release/librunic.so ./my_c_program
```

[mimalloc-bench](https://github.com/daanx/mimalloc-bench):

```sh
alloc_lib_add "runic" "/path/to/runic/target/release/librunic.so"
./bench.sh runic cfrac espresso
```

Do not enable `c-abi` on ordinary `runic-alloc` dependents: `#[no_mangle] malloc` would replace libc in that binary. Workspace `cargo test` stays on the default features.

## Crate Shape

- `src/lib.rs`: public export surface.
- `src/global.rs`: configured `RunicAlloc` implementation of `GlobalAlloc`.
- `src/cabi.rs`: C malloc-family intercepts; `#[no_mangle]` only with `c-abi`.
- `src/bin/abort_case.rs`: subprocess binary used by abort tests.
- `tests/`: global allocator smoke and abort-case integration tests.

Most allocator mechanics live in `runic-core`.
