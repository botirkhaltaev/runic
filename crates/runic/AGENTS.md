# AGENTS.md

Scope: `crates/runic/`.

- Public surface is `RunicAlloc` (`runic-alloc` package / `runic` library name).
- `GlobalAlloc` methods delegate to `runic_core::Allocator`; do not duplicate core policy or abort logic.
- Abort cases → subprocess tests (`crates/runic/tests/`).
