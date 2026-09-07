# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate the exact pointer **before** any state CAS.
- `ExtentCache` is an intrusive list (`head` / `Extent::next`) of published Free arena extents, not raw `Mapping`s after unpublish. `Budget::slots` is exact.
- `ExtentHeap::{free,accept}` → domain op then `cache_or_unmap` (`unmap` on miss / over-budget).
- Keep retention never evicts a cached extent to admit another; reuse is exact mapping length only.
- Details: `crates/runic-core/src/heap/extent/README.md`.
