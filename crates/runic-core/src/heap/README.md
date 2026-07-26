# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap table / thread binding.

## Layout

- `mod.rs`: `Heap` (mode, runs, extents, `Inbox`); small path via `acquire_run` / `alloc_run`; sticky TLS hits call `Run::{allocate,free}` directly; owner free is `Heap::free(PageOwner)`.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapTable` (`acquire`/`retire`/`reclaim`, `heap`/`mode`, `publish`), `Inbox`, and `ThreadHeap` (`bind`/`alloc`/`dealloc`/`free`/`alloc_extent`/`free_extent`/`batch`).

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees use `claim` → inbox enqueue → owner (or draining) `accept` via flush; they do not mutate freelists directly.
- Draining reclaim observes live ownership on the heaps themselves via `RunHeap::has_live_blocks` and `ExtentHeap::has_live_extents` (composed by `Heap::has_live_allocations`). There is no side allocation counter on `Heap`.
- Owner free composition stays on `Heap::free(PageOwner)` / `flush`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Allocator routing is one-TLS `dealloc` → `ThreadHeap::dealloc` (`id_for(layout)` → sticky `Run::free`, else PageMap → `free` / `free_extent`) / `Allocator::free_remote`.
- `Heap` modes: `Free` (reusable), `Active` (TLS owner), `Draining` (post-exit until empty).
- `HeapTable::generations[]` owns `HeapId` ABA / reincarnation checks.
