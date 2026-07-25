# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap table / thread binding.

## Layout

- `mod.rs`: `Heap` (mode, runs, extents, `Inbox`); small path via `acquire_run` / `alloc_run`; sticky TLS hits call `Run::{allocate,free_local}` directly.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapTable` (`acquire`/`retire`/`reclaim`, `heap`/`mode`, `publish`), `Inbox`, and `ThreadHeap` (`bind`/`alloc`/`free`/`alloc_extent`/`free_extent`/`batch`).

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees use claim → inbox enqueue → owner (or draining) flush; they do not mutate freelists directly.
- Draining reclaim observes live ownership on the heaps themselves: any run with `live > 0` (allocated or remote-pending) or any occupied extent. There is no side allocation counter on `Heap`.
- `Heap` modes: `Free` (reusable), `Active` (TLS owner), `Draining` (post-exit until empty).
- `HeapTable::generations[]` owns `HeapId` ABA / reincarnation checks.
