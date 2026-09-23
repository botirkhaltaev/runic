# AGENTS.md

Scope: `crates/runic/`.

- Public surface is `RunicAlloc` (`runic-alloc` package / `runic` library name).
- `GlobalAlloc` methods delegate to `runic_core::Allocator`; do not duplicate core policy or abort logic.
- C intercepts live in `cabi`. `#[no_mangle]` only with feature `c-abi`. Do not install `#[global_allocator]` in the cdylib. `free(NULL)` is a C no-op; `Allocator::free` still aborts on null.
- Abort cases → subprocess tests (`crates/runic/tests/`).
