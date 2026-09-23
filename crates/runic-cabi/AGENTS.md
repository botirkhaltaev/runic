# AGENTS.md

Scope: `crates/runic-cabi/`.

- This package is cdylib-only. It always exports the C malloc family and must never install `#[global_allocator]`.
- Entry points translate C contracts at the boundary, then delegate to `runic_core::Allocator`.
- `free(NULL)` is a no-op. `Allocator::free(NULL)` still aborts.
- Pointer-only `free` / `realloc` recover ownership through `PageMap`; never guess a layout for `Run::header_of`.
- Unit tests cover the C contracts in process; `runic-preload` covers interposition, thread exit, aborts, and the exported symbol set.
- Adding or removing an export means updating `EXPORTS` in `runic-preload`.
