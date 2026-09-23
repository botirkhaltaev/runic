# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `AllocatorCtx`), `heaps.rs`, `state.rs`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: `Arena<Heap>`; `get` is lock-free `Arena` then `Heap::matches` (slot + generation). Free heaps are an intrusive index freelist. Owner give-up is `unbind` (close Active, wait leases, flush/accept/reclaim). Draining API: `free` / `flush` (`owner: Option` — `Some` uses `owner.heap()`, `None` `get`s). Fail when the OS will not map or the arena is full.
- Exclusive metadata is `Mutex<HeapInner>`. Active: `require_inner` (`try_inner` or abort), from any TLS `ThreadHeap`. `Heap::adopt` locks before its lifecycle CAS and returns the guard for the first flush, serializing with Draining reclaim. Draining: `Heaps::admit` / `Heap::admit` then `flush` (`owner` queues a claim before accept). `AllocatorCtx` is the parent bag, not a lock guard. `ThreadHeaps::unbind` every slot.
- `THREAD_HEAPS` is `#[thread_local]` `!Drop` ELF TLS. `UnbindGuard` is the only `LocalKey` (touched in `bind`; `Drop` unbinds every slot).
- Details: `crates/runic-core/src/heap/README.md`.
