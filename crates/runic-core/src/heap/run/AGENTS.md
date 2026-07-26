# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump own Free/Live; bump allocate stores no `BlockStates`.
- `BlockState` clear / Free / `RemotePending` — Free bit is DF fail-closed only, not a second Free/Live authority. Query via `BlockStates::state` + match; no `is_*` bool helpers.
- Sticky TLS: `Run::allocate` / `Run::free` in-line; empty sticky → `acquire_alloc`. No sticky `finish_free`.
- `Heap::{acquire_run,alloc_run}` compose small alloc; no `take_or_*` / `alloc_from` forks.
- `block_shift: Option<NonZeroU32>` — never a `0` sentinel.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
