# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- Keep TLS heap-entry owner-local frontend state and run caches here.
- `Inbox` is a movable Treiber-style head of intrusive `RemoteList` batches; construct with `Inbox::new()`.
- Remote frees use batched transport: `RemoteBatch` on `ThreadHeap` coalesces; `Inbox::push_batch` / `drain` move `RemoteList`s. Do not add single-node push/pop façades.
- Create heaps with `Heap::new` + `Arena::claim` / `insert` (inbox is movable; no placement-only install).
- Keep `Heap` responsible for Free/Active/Draining mode and owner-local lifecycle helpers.
- Keep `HeapTable` responsible for slot identity, `generations[]` ABA checks, `push_remote_batch`, and reclaim generation bumps.
- Do not put `HeapTable` on steady-state owner-local allocation hot paths.
- Clear or validate owner-local caches whenever a heap is abandoned or reactivated.
- Preserve explicit separation between owner-local frees and remote-free claim→batch→inbox→drain behavior.
- Do not introduce passive forwarding wrappers for heap table behavior; prefer methods on `HeapTable`, `Heap`, or the TLS heap entry that owns the state.
