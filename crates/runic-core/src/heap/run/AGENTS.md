# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via `Arena<Run>` (`claim` / `insert` / `release` / `remove`).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap), `alloc_from` (one block), and one-shot `alloc_run` for the locked non-sticky path. Do not reintroduce a `take_or_*` fork.
- `ThreadHeap::alloc` shares those primitives: sticky hit via `alloc_from`; miss flushes at most once then retries sticky before `acquire_run`.
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- `BlockStates` is one `AtomicU8` per block slot, sized to the smallest class worst case — not a packed bitmap. It is the only free/allocated/remote-pending tracker; do not add a second freelist or bitset.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
