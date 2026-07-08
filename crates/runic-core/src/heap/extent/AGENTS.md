# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- Keep exact-pointer validation on `Extent`.
- Keep dedicated allocation policy and mapping reuse on `ExtentHeap`.
- Keep metadata storage and reservation behavior on `ExtentArena`.
- Remove page-map ownership before removing extent metadata.
- Avoid unbounded mapping retention; preserve `ExtentCache` slot and byte limits.
- Add tests beside the extent entity that owns the changed invariant.
