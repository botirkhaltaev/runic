# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump + `live` own Free/Live. No per-block state byte.
- Remote admission: private claim bitmap in the mapping tail (`issued` + `try_set`).
- Domain ops on `Run`: `allocate` (refill) / `free` (`locate` + push) / `claim` / `accept` (clears queued then drains claim words). Owner DF undefined.
- Embedded `InboxLink` coalesces by run (one inbox entry for many claims).
- `RunHeap::acquire` (available or cold mmap) backs slot `acquire_run`; no `take_or_*` / `alloc_from` forks.
- Geometry on `Run` (`stride` / `stride_shift` / `address`); `locate` → `SizeClass::index_of` after span check.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
