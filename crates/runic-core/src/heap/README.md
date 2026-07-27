# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap directory / thread binding.

## Layout

- `mod.rs`: `Heap` (runs, extents, `HeapId` only); small path via `acquire_run`.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`, `ClaimBits`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapDirectory` (lock-free `slot`, internally locked lifecycle), `HeapSlot` (`SlotState`, run/extent inboxes, `UnsafeCell<Heap>`), `Inbox`/`Notify`, `ThreadHeap`.

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` on the run/extent → `try_arm` on embedded `Notify` → immediate `HeapDirectory::publish_on` when the arm wins (Active lease or Draining locked accept). Coalescing is by owner: many remote frees against one run collapse to one inbox entry. Owner `flush` drains via `Run::accept_remote` / extent accept.
- Run remote admission is exclusively `ClaimBits` (mapping-tail bitmap). Owner `Run::free` stores Free then rechecks the claim bit (no owner `lock cmpxchg`). Extents still use byte `RemotePending`.
- `Inbox::republish` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap::has_live_blocks` and `ExtentHeap::has_live_extents` (composed by `Heap::has_live_allocations`). In-flight claim bits keep the heap live.
- Never-bound freers publish each successful claim in `Allocator::free_remote` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free composition stays on `HeapSlot::free` / `flush`; domain ops are `free` / `claim` / `accept_remote` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Sticky miss prefers local/OS `acquire_run` before inbox flush so lock-free remote fan-in does not force the owner onto accept every miss.
- `SlotState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **publisher lease** count for Active **publish** admits only (not inbox depth — that stays live via claim bits / `has_live_allocations`).
- `HeapSlot` heap metadata is behind `UnsafeCell`: Active TLS owner or directory-locked Draining/Free only. No whole-slot `&mut` after publication.
- `HeapDirectory` publishes stable slot pointers per index once; lookup is lock-free. Lifecycle mutex is private inside the directory facade.
