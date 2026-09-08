# runic-alloc

`runic-alloc` is Runic's public `GlobalAlloc` wrapper crate.

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
budgets allow it, `Drop` retains nothing. Allocation-side lookup always reuses a
retained mapping by exact length. Slot and byte budgets are enforced exactly.

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

## Crate Shape

- `src/lib.rs`: public export surface.
- `src/global.rs`: configured `RunicAlloc` implementation of `GlobalAlloc`.
- `src/bin/abort_case.rs`: subprocess binary used by abort tests.
- `tests/`: global allocator smoke and abort-case integration tests.

Most allocator mechanics live in `runic-core`.
