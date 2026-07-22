# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- Keep TLS heap-entry owner-local frontend state and run caches here.
- `Inbox` is a movable Treiber-style head of intrusive `RemoteList` batches; construct with `Inbox::new()`.
- `RemoteList.first`/`.last` are plain `NonNull<u8>`, not `Option`: a list is only ever built from a non-empty batch, so construction (`RemoteList::from_ends`) and `Inbox::push_batch` never need to check or `expect` non-emptiness.
- Remote frees use batched transport: `RemoteBatch` on `ThreadHeap` coalesces; `Inbox::push_batch` / `drain` move `RemoteList`s. Do not add single-node push/pop façades.
- Create heaps with `Heap::new` + `Arena::claim` / `insert` (inbox is movable; no placement-only install).
- Keep `Heap` responsible for Free/Active/Draining mode and owner-local lifecycle helpers; `Heap::mode()` returns the `HeapMode` snapshot directly (Free/Active/Draining) for callers that must branch on lifecycle state.
- Keep `HeapTable` thin: slot identity (`slots` arena + `generations[]`), `acquire` / `retire` / `try_reclaim`, and mode-aware `publish`. Do not put allocate/dealloc routers on the table.
- `HeapTable::publish` under the table lock: `Active` enqueues to the inbox; `Draining` enqueues then `flush`es and may reclaim; `Free`/stale generation fails. Retained TLS batches must stay publishable after owner exit.
- Do not put `HeapTable` on steady-state owner-local allocation hot paths (TLS cached alloc/free must not take the table mutex).
- Owner non-cached free is `Heap::free_run_owner` / `free_extent_owner` (may flush inbox); TLS cached free stays `Run::free_local` + `release_allocation`.
- Clear or validate owner-local caches whenever a heap is abandoned or reactivated.
- Preserve explicit separation between owner-local frees and remote-free claim→batch→`publish`→drain behavior. There is exactly one remote-free protocol (`Allocator`'s slow dealloc path claims, coalesces onto the TLS `RemoteBatch`, and calls `publish`; draining completion under the table lock uses `Heap` free + `try_reclaim`). Do not add a second, unbatched remote-free implementation for `realloc` or any other caller — route all cross-heap frees (including from `realloc`) through the same `Allocator::dealloc` path.
- Do not introduce passive forwarding wrappers for heap table behavior; prefer methods on `HeapTable`, `Heap`, or the TLS heap entry that owns the state.
