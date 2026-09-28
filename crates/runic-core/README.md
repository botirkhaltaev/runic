# runic-core

Allocator mechanics. Most modules are crate-private. The public type is
`runic_core::Allocator`.

Depend on `runic-alloc` unless you are embedding the core (as `runic-cabi`
does).

```toml
[dependencies]
runic-alloc = "0.8"
```

## Contract

- `alloc` / `dealloc` / `alloc_zeroed` take `Layout`. `dealloc` null aborts.
- `free` / `resize` / `usable_size` are pointer-only. Owner comes from
  `PageMap`. `header_of` is not used here (extent mappings may omit the run
  header page). `free` null aborts. C `free(NULL)` is handled in `runic-cabi`.
- First `init` in the process wins. `Allocator::preload()` overlays `RUNIC_*` at
  that init (cabi). `RunicAlloc::new().with_*` does not read env.
- `safe` and `hardened` are exclusive Cargo features. Fast leaves owner
  double-free undefined. `hardened` adds a freelist cookie, a slot canary,
  extent guard pages, a 256 KiB delay, and header checksums.
- Thread exit is `ExitHook`, armed once per thread. Feature `c-abi` registers
  it as a pthread key for `LD_PRELOAD`; default is a `std::thread_local!` guard.

Safety: returned memory is uninitialized unless `alloc_zeroed`. The caller must
pass a live pointer back. Invalid domain state after lifecycle retries calls
`Allocator::abort`.

Modules: [src/README.md](src/README.md). API:
[docs.rs/runic-core](https://docs.rs/runic-core). Design:
[ARCHITECTURE.md](../../ARCHITECTURE.md).
