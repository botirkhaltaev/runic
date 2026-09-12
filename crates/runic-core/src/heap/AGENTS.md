# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `AllocatorCtx`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: `Arena<Heap>`; `get` is lock-free `Arena` then `state.matches`. Free heaps are an intrusive index freelist. Draining API: `enqueue` / `free` / `flush` / `reclaim`. Fail when the OS will not map or the arena is full.
- Exclusive metadata is `Mutex<HeapInner>`. Active: `require_inner` (`try_inner` or abort), including an adopted heap. `adopt` flush is the one `lock_inner` wait for an in-flight Draining admit. Draining: `lock_inner` after mode check. `AllocatorCtx` is the parent bag, not a lock guard. `unbind` retires bound and adopted.
- `THREAD_HEAP` is `#[thread_local]` `!Drop` ELF TLS. `UnbindGuard` is the only `LocalKey` (touched in `bind`; `Drop` retires the heap).
- Details: `crates/runic-core/src/heap/README.md`.
