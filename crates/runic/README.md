# runic-alloc

Public `GlobalAlloc` wrapper. Package `runic-alloc`, library `runic`.

```sh
cargo add runic-alloc
```

```rust
use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();
```

`dealloc` requires a live pointer this allocator returned. Null aborts
(`GlobalAlloc` contract). Unknown and interior pointers abort.

## Config

First `init` in the process wins; later configs are ignored. Extent policy
applies on free: `Keep` retains a mapping while slot and byte budgets allow,
`Discard` retains then `madvise`s, `Unmap` does not retain. Allocate-side reuse
is exact mapping length.

```rust
use runic::{Budget, ExtentPolicy, RunPolicy, RunicAlloc};

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Keep)
    .extent_budget(Budget::new(64, 64 * 1024 * 1024))
    .run_policy(RunPolicy::Keep)
    .build();
```

C `LD_PRELOAD` is the `runic-cabi` package, not a feature of this crate.

Nightly Rust on Linux `x86_64`. API:
[docs.rs/runic-alloc](https://docs.rs/runic-alloc). Design:
[ARCHITECTURE.md](../../ARCHITECTURE.md). Gaps:
[COMPATIBILITY.md](../../COMPATIBILITY.md).
