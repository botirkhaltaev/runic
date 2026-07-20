# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap table / thread binding.

## Layout

- `mod.rs`: `Heap` (mode, runs, extents, `alloc_count`, `Inbox`) and exclusive allocate/free/flush.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapTable` (`Arena<Heap>`, `generations[]`), `Inbox`, and `ThreadHeap`.

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees use claim → inbox enqueue → owner (or draining) flush; they do not mutate freelists directly.
- `alloc_count` tracks outstanding allocations (including remote-pending) for reclaim safety.
- `Heap` modes: `Free` (reusable), `Active` (TLS owner), `Draining` (post-exit until empty).
- `HeapTable::generations[]` owns `HeapId` ABA / reincarnation checks.
