# AGENTS.md

Scope: `crates/runic/`.

- Public surface is `RunicAlloc` (`runic-alloc` package / `runic` library name).
- `GlobalAlloc` methods delegate to `runic-core::Allocator`; do not duplicate core policy.
- Abort cases → subprocess tests (`crates/runic/tests/`).
