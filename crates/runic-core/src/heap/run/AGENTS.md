# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump own Free/Live; bump allocate stores no `BlockStates`.
- Remote admission is a private claim bitmap in the run mapping tail; `BlockStates` is Clear/Free only (no owner `cas`).
- Domain ops stay on `Run` (`allocate`/`free`/`claim`/`accept`); free is store+recheck, claim is set-bit+recheck, `accept` clears inbox queued then drains all claim words in one pass.
- Embedded `InboxLink` coalesces remote frees by run — one inbox entry regardless of how many blocks are claimed before the owner drains.
- Sticky TLS: `Run::allocate` / `Run::free` in-line; empty sticky → `#[cold] refill_sticky` (local/OS then flush). No sticky available-list relink.
- `RunHeap::acquire` (available list or cold mmap) backs `HeapSlot::{acquire_run,alloc_run}`; no `take_or_*` / `alloc_from` forks.
- `Run` owns span/capacity and stride geometry (`stride` / `stride_shift` / `address`); `locate` → `SizeClass::index_of` after span check.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
