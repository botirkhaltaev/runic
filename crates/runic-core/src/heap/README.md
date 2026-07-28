# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `LockedHeap`, and re-exports.
- `heaps.rs`: `Heaps` (index/publish).
- `state.rs`: `HeapMode`, `HeapState`, `Lease`.
- `inbox.rs`: `Inbox` / `InboxLink`.
- `thread.rs`: `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active push-or-coalesce; lease only if newly queued) or `Heaps::lock` → `LockedHeap` (exclusive late free / link). Coalescing is by owner: many remote frees against one run collapse to one inbox entry. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the mapping tail. Owner `Run::free` stores Free then rechecks the claim bit (no owner `lock cmpxchg`). Extents use byte `Claimed`.
- `Inbox::push` / `link` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap` ∨ `ExtentHeap` (`has_live`). In-flight claim bits keep the heap live.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free composition stays on `Heap::free` / `flush` (via `ThreadHeap` or `LockedHeap`); domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Sticky-empty refill prefers local/OS `acquire_run` before inbox flush so lock-free remote fan-in does not force the owner onto accept every refill. Sticky hit: no locks, no atomics.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- Active mutation is TLS-exclusive (`ThreadHeap`); Draining mutation requires `LockedHeap`. Shared freers only touch atomics (`state`, inboxes).
- `Heaps` publishes stable heap pointers per index once; `get` is lock-free. Arena mutex covers claim/reuse only — never flush/accept.
