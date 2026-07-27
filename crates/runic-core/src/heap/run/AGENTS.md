# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump own Free/Live; bump allocate stores no `BlockStates`.
- `BlockStates`: `state` / `set` / `cas` only. Domain ops stay on `Run` (`allocate`/`free`/`claim`/`accept`).
- Sticky TLS: `Run::allocate` / `Run::free` in-line; empty sticky → `acquire_alloc`. No sticky `finish_free`.
- `Heap::{acquire_run,alloc_run}` compose small alloc; no `take_or_*` / `alloc_from` forks.
- Run payload block index/offset via `SizeClassId::{block_index_from_offset,block_offset}` (27-arm constant divisors).
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
