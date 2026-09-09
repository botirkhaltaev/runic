# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `AllocatorCtx`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: lock-free `chunks` + `len`; `get` is two Acquire loads then `state.matches` (slots never move). `grow` mutex owns mappings and bump insert. Free heaps are an intrusive index freelist. Draining API: `enqueue` / `free` / `flush` / `reclaim`. No product heap cap.
- Exclusive metadata is `Mutex<HeapInner>`. Active: `try_inner` (fail → abort). Draining: `lock_inner` after mode check. `AllocatorCtx` is the parent bag, not a lock guard.
- `THREAD_HEAP` is `#[thread_local]` `!Drop` ELF TLS. `UnbindGuard` is the only `LocalKey` (touched in `bind`; `Drop` retires the heap).
- Details: `crates/runic-core/src/heap/README.md`.
