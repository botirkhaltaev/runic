# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `LockedHeap`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: lock-free `get`; `arena` mutex only for `acquire` / Free reactivation; `lock` is the sole `LockedHeap` constructor. No body mutation.
- Shared `&Heap`: atomics only (`enqueue`, `is_active` / `mode` / `close`). No `state()` projection; no public body ops.
- `ThreadHeap`: sole Active body path (`alloc` / `free_*` / `flush` / `alloc_*_after_bind`). Sticky hit: no locks/atomics.
- `LockedHeap`: sole Draining body path + reclaim on Drop.
- Details: `crates/runic-core/src/heap/README.md`.
