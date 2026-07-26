# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via grow-on-demand `Arena<Run>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap) and one-shot `alloc_run` for the locked non-sticky path. Do not reintroduce a `take_or_*` fork or an `alloc_from` pass-through.
- `ThreadHeap::alloc` sticky hit calls `Run::allocate` in-line (symmetric with sticky `Run::free`); miss (cold) flushes at most once, retries sticky, then `acquire_run` and parks the run.
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- `BlockStates` is one `AtomicU8` per block for this run's capacity, an `AddressRange` over the run mapping's state tail after the `RUN_SIZE` payload — not a packed bitmap and not a worst-case inline array. It is the only free/allocated/remote-pending tracker; do not add a second freelist or bitset.
- Freelist identity is `Option<BlockIndex>` (head + intrusive next index in the block). `allocate` pops/bumps an index then `block_ptr`; do not store raw user pointers in the freelist head or add trusted-pointer helper forks. `free` / `claim` / `accept` validate user pointers with `block_at`.
- Domain free ops: `Run::free` (Allocated→Free), `claim` (Allocated→RemotePending), `accept` (RemotePending→Free), `unclaim` (rollback). `RunHeap::{free,accept}` share private `finish_free` for available-list bookkeeping; sticky TLS free calls `Run::free` only (no `finish_free`).
- `Run::has_live_blocks` / `RunHeap::has_live_blocks` are the reclaim model for outstanding small ownership (including remote-pending); do not reintroduce a heap-level alloc side counter.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
