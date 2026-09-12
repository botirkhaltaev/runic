# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate the exact pointer **before** any state CAS.
- `ExtentConfig` / `ExtentPolicy` live in `config.rs`. `ExtentCache` is an intrusive list (`head` / `Extent::next`) of published Free arena extents, not raw `Mapping`s after unpublish. `Budget::slots` is exact.
- `ExtentHeap::{free,accept}` → domain op then `cache_or_unmap` (`insert` or `unmap`).
- Keep/Discard retention never evicts a cached extent to admit another; reuse is exact mapping length only. `Extent::discard` is Discard cache-insert `madvise` only. Keep Zeroed reuse ≥256 KiB discards pages without that flag, else memset. `resize_in_place` refuses a size-class spec.
- Details: `crates/runic-core/src/heap/extent/README.md`.
