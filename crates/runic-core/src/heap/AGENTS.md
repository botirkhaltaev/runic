# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `HeapCtx` / `HeapsCtx`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: `RwLock<Arena<Heap>>`; `get` is a short read lock then `&Heap` (slots never move); write lock only for `acquire` / Free reactivation; Free heaps are an intrusive index freelist (no `0..N` scan). Draining API: `enqueue` / `free` / `flush` / `reclaim`. No product heap cap.
- Shared `&Heap`: atomics only (`enqueue`, `is_active` / `mode` / `close`). No `state()` projection; no public body ops.
- Exclusive metadata is `Mutex<HeapInner>`. Active: `try_inner` (fail → abort). Draining: `lock_inner` after mode check. Ctx is a parent bag (`HeapCtx { pages }`, `HeapsCtx { heaps, pages }`), not a lock guard.
- `ThreadHeap`: sole Active body path (`alloc` / `free_*` / `flush` / `alloc_*_after_bind`). Hit: current-run pop / run-cache `Run::free` (`push_available` only on `was_full`). `#[cold]` only abort / bind / map / remote / unbind.
- `THREAD_HEAP` has no `Drop`; `UnbindGuard` TLS retires on thread exit (touched in `bind`).
- Details: `crates/runic-core/src/heap/README.md`.
