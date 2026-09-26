# AGENTS.md

Scope: `crates/runic-core/src/heap/` (not `run/` / `extent/` — those have their own).

- Flat layout: `mod.rs` (`Heap` / `HeapInner` / `AllocatorCtx`), `heaps.rs`, `state.rs`, `list.rs`, `queue/`, `inbox.rs`, `thread.rs`, plus `run/` / `extent/`.
- `Heaps`: `Arena<Heap>`; `get` is lock-free `Arena` then `Heap::matches` (slot + generation). Free heaps are an `Mpmc` stack. Owner give-up is `unbind` (close Active, wait leases, flush/accept/reclaim). Draining API: `free` / `flush` (`owner: Option`. `Some` uses `owner.heap()`, `None` `get`s). Fail when the OS will not map or the arena is full.
- Exclusive metadata is `Mutex<HeapInner>`. One guard at a time, for the heap whose available runs or extent cache is changing. Active accept walks the inbox outside the lock; `require_inner` publishes (`push_available` / `cache_or_unmap`) and alloc-miss then `acquire_run` under that same guard. `Heap::adopt` locks only for the lifecycle CAS. Draining: `Heaps::admit` / `Heap::admit` then `flush` under one guard through reclaim (`owner` queues a claim before accept). `AllocatorCtx` is the parent bag, not a lock guard. `ThreadHeaps::unbind` pops the list. The alloc heap is the front. Adopt links at the back. `idle` uses `inboxes_empty` and `occupied` and unlinks one idle heap only while another remains.
- `THREAD_HEAPS` is `#[thread_local]` `!Drop` ELF TLS. Default builds use `UnbindGuard`; `c-abi` builds use `UnbindHook`, a once-per-thread `pthread` key because Rust TLS destructor registration allocates and re-enters `bind` under `LD_PRELOAD`.
- Details: `crates/runic-core/src/heap/README.md`.
