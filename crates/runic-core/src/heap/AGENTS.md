# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `AllocatorCtx`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: `RwLock<Arena<Heap>>`; `get` is a short read lock then `&Heap` (slots never move); write lock only for `acquire` / Free reactivation; Free heaps are an intrusive index freelist. Draining API: `enqueue` / `free` / `flush` / `reclaim`. No product heap cap.
- Exclusive metadata is `Mutex<HeapInner>`. Active: `try_inner` (fail → abort). Draining: `lock_inner` after mode check. `AllocatorCtx` is the parent bag, not a lock guard.
- `THREAD_HEAP` has no `Drop`; `UnbindGuard` TLS retires on thread exit (touched in `bind`).
- Details: `crates/runic-core/src/heap/README.md`.
