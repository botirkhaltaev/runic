# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + `live` own Free/Live. `allocate` is pop only. `extend` threads one page (min 32) onto the freelist; `issued` advances once.
- Remote admission: private claim bitmap in the mapping tail (`issued` + `try_set`).
- Domain ops on `Run`: `allocate` / `extend` / `free` (`locate` + push) / `claim` / `accept`. Owner DF undefined.
- Embedded `InboxLink` coalesces by run (one inbox entry for many claims).
- `RunHeap::acquire` (available or cold mmap) backs slot `acquire_run`; no `take_or_*` / `alloc_from` forks.
- Geometry on `Run` (`span` / `recip` / `stride` / `address`); `locate` is span + reciprocal divisibility (rejects interior and tail slack).
- `RunCache` owns the TLS free probe (`hit` / `store` / `clear`). `ExtentCache` is heap retention — not a second TLS slot.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
