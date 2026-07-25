# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via grow-on-demand `Arena<Run>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap), `alloc_from` (one block), and one-shot `alloc_run` for the locked non-sticky path. Do not reintroduce a `take_or_*` fork.
- `ThreadHeap::alloc` sticky hit calls `Heap::alloc_from` in-line; miss (cold) flushes at most once, retries sticky, then `acquire_run` and parks the run.
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- `BlockStates` is one `AtomicU8` per block for this run's capacity, an `AddressRange` over the run mapping's state tail after the `RUN_SIZE` payload — not a packed bitmap and not a worst-case inline array. It is the only free/allocated/remote-pending tracker; do not add a second freelist or bitset.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
