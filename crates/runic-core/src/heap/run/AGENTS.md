# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump own Free/Live; bump `allocate` does not store `BlockStates`.
- Remote admission: private claim bitmap in the mapping tail; `BlockStates` is Clear/Free only (no owner `cas`).
- Domain ops on `Run`: `allocate` / `free` / `claim` / `accept` (free = store+recheck; claim = set-bit+recheck; `accept` clears queued then drains claim words).
- Embedded `InboxLink` coalesces by run (one inbox entry for many claims).
- `RunHeap::acquire` (available or cold mmap) backs slot `acquire_run` / `alloc_run`; no `take_or_*` / `alloc_from` forks.
- Geometry on `Run` (`stride` / `stride_shift` / `address`); `locate` → `SizeClass::index_of` after span check.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
