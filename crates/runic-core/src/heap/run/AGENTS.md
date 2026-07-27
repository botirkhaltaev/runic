# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + bump own Free/Live; bump allocate stores no `BlockStates`.
- `ClaimBits` exclusively owns remote admission; `BlockStates` is Clear/Free only (no `RemotePending`, no owner `cas`).
- Domain ops stay on `Run` (`allocate`/`free`/`claim`/`accept_remote`); free is store+recheck, claim is set-bit+recheck, `accept_remote` drains all claim words in one pass.
- `Run::try_arm` / the embedded `Notify` coalesce remote frees by run — one inbox entry regardless of how many blocks are claimed before the owner drains.
- Sticky TLS: `Run::allocate` / `Run::free` in-line; empty sticky → `acquire_alloc`. No sticky `finish_free`.
- `Heap::{acquire_run,alloc_run}` compose small alloc; no `take_or_*` / `alloc_from` forks.
- `Run` owns span/capacity and stride geometry (`stride` / `stride_shift` / `address`); `locate` → `SizeClass::index_of` after span check.
- Runs retained in v0.5 (no empty-run unpublish). Details: `crates/runic-core/src/heap/run/README.md`.
