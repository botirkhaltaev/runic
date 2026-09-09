# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate the exact pointer **before** any state CAS.
- `ExtentConfig` / `ExtentPolicy` live in `config.rs`. `ExtentCache` is an intrusive list (`head` / `Extent::next`) of published Free arena extents, not raw `Mapping`s after unpublish. `Budget::slots` is exact.
- `ExtentHeap::{free,accept}` → domain op then `cache_or_unmap` (`insert` or `unmap`).
- Keep/Discard retention never evicts a cached extent to admit another; reuse is exact mapping length only. `Extent::discard` is cache-insert `madvise`; Zeroed reuse skips memset when that advise succeeded.
- Details: `crates/runic-core/src/heap/extent/README.md`.
