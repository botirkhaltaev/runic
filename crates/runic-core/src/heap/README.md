# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap directory / thread binding.

## Layout

- `mod.rs`: `Heap` (runs, extents, `HeapId` only); `HeapMode`; small path via `acquire_run` / `alloc_run`.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `table/`: `HeapDirectory` (lock-free `slot`, internally locked lifecycle), `HeapSlot` (`SlotState`, inbox, `UnsafeCell<Heap>`), `Inbox`, `ThreadHeap`.

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: Active `claim` → TLS batch → publish only on flush (capacity, target change, Draining observation, unbind, or never-bound singleton) via `HeapDirectory::publish` (Active lease or locked Draining accept). Bound coalesce-only frees do not take a publisher lease. Draining late frees use locked `free_draining`. Owner `flush` → `accept`.
- `Inbox::push_batch` is a Treiber CAS loop: store `last.next = old_head` before CASing `head: old → first`, so drain never observes a published head without the prior chain. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap::has_live_blocks` and `ExtentHeap::has_live_extents` (composed by `Heap::has_live_allocations`). Claimed / remote-pending blocks keep the heap live.
- Partial TLS remote batches flush on capacity, target change, a later Draining observation, or bound unbind. Never-bound freers publish each claim in `Allocator::free_remote` (no Drop-stranded `RemotePending`). An idle long-lived **bound** producer may delay reclaim (bounded one-batch-per-TLS backpressure).
- Owner free composition stays on `HeapSlot::free` / `flush`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback/`unclaim`).
- Sticky miss prefers local/OS `acquire_run` before inbox flush so lock-free remote fan-in does not force the owner onto accept every miss (measured fan-in win).
- `SlotState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **publisher lease** count for Active **publish** admits only (not unpublished TLS batch size; those stay live via `RemotePending` / `has_live_allocations`). Admission closes at Active→Draining CAS; Active publish linearizes at inbox head CAS under a held lease (inbox is private — no lease-skipping push surface).
- `HeapSlot` heap metadata is behind `UnsafeCell`: Active TLS owner or directory-locked Draining/Free only. No whole-slot `&mut` after publication.
- `HeapDirectory` publishes stable slot pointers per index once; lookup is lock-free. Lifecycle mutex is private inside the directory facade.
