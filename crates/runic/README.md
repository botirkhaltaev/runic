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

First `init` in the process wins; later configs are ignored. `new()` is Fast
with hugepage and NUMA off. `with_mode` other than Fast aborts at init (Safe
and Hardened are not shipped). Extent policy applies on free: `Keep` retains a
mapping while slot and byte budgets allow, `Discard` retains then `madvise`s,
`Unmap` does not retain. Allocate-side reuse is exact mapping length. Payload
maps honor hugepage (`Off` / `Thp`) and NUMA (`Off` / `Local`).

```rust
use runic::{Budget, ExtentConfig, ExtentPolicy, HugePage, Numa, RunConfig, RunPolicy, RunicAlloc};

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new()
    .with_hugepage(HugePage::Thp)
    .with_numa(Numa::Local)
    .with_extent_config(
        ExtentConfig::new()
            .with_policy(ExtentPolicy::Keep)
            .with_budget(Budget::new(64, 64 * 1024 * 1024)),
    )
    .with_run_config(RunConfig::new().with_policy(RunPolicy::Keep));
```

This `with_*` chain is const and does not read the environment. Preload uses
`RUNIC_*` (see `runic-cabi`).

C `LD_PRELOAD` is the `runic-cabi` package, not a feature of this crate.

Nightly Rust on Linux `x86_64`. API:
[docs.rs/runic-alloc](https://docs.rs/runic-alloc). Design:
[ARCHITECTURE.md](../../ARCHITECTURE.md). Gaps:
[COMPATIBILITY.md](../../COMPATIBILITY.md).
