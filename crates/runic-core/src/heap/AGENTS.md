# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `LockedHeap`), `heaps.rs` (`Heaps`), `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: lock-free `get`; `arena` mutex only for `acquire` / Free reactivation. Does not flush or mutate run/extent metadata.
- `Heap`: lifecycle + inboxes + `id`/`RunHeap`/`ExtentHeap`; shared `enqueue`; Active via `ThreadHeap`; Draining via `LockedHeap`.
- `LockedHeap`: exclusive Draining capability; Drop → `reclaim`. Not the heaps arena mutex.
- `ThreadHeap`: sticky hit has no locks/atomics; sticky empty → `#[cold] refill`.
- Details: `crates/runic-core/src/heap/README.md`.
