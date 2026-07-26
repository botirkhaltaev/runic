# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap directory / thread binding.

## Layout

- `mod.rs`: `Heap` (runs, extents, `HeapId` only); `HeapMode`; small path via `acquire_run` / `alloc_run`.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapDirectory` (`acquire`/`retire`/`reclaim`/`publish`, `slot`), `HeapSlot` (`HeapRoute`, inbox, publishers, heap), `Inbox`, `ThreadHeap`.

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees use `claim` → inbox enqueue → owner (or draining) `accept` via flush; they do not mutate freelists directly.
- `Inbox::push_batch` is a Treiber CAS loop: store `last.next = old_head` before CASing `head: old → first`, so drain never observes a published head without the prior chain.
- Draining reclaim observes live ownership on the heaps themselves via `RunHeap::has_live_blocks` and `ExtentHeap::has_live_extents` (composed by `Heap::has_live_allocations`). There is no side allocation counter on `Heap`.
- Owner free composition stays on `HeapSlot::free` / `flush`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Allocator routing is one-TLS `dealloc` → `ThreadHeap::free` / `free_extent` / `Allocator::free_remote`.
- `HeapRoute` modes: `Free` (reusable), `Active` (TLS owner), `Draining` (post-exit until empty). Generation overflow permanently retires the slot.
- `HeapSlot` is the sole lifecycle authority (route + inbox); `HeapDirectory` publishes stable slot pointers per index.
