# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and the heap directory / thread binding.

## Layout

- `error.rs`: `HeapError` at the slot edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (slot index + generation).
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).
- `directory/`: `mod.rs` (`HeapDirectory`), `state.rs` (`HeapMode`, `SlotState`, `PublisherLease`), `slot.rs` (`HeapSlot`, `LockedSlot`), `inbox.rs` (`Inbox`/`InboxLink`), `thread.rs` (`ThreadHeap`).

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `HeapSlot` owns run/extent metadata directly (no thin `Heap` shell).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `HeapSlot::enqueue` (Active push-or-coalesce; lease only if newly queued) or `HeapDirectory::lock` → `LockedSlot` (exclusive late free / link). Coalescing is by owner: many remote frees against one run collapse to one inbox entry. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the mapping tail. Owner `Run::free` stores Free then rechecks the claim bit (no owner `lock cmpxchg`). Extents use byte `Claimed`.
- `Inbox::push` / `link` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via private `SlotHeap::has_live` (`RunHeap` ∨ `ExtentHeap`). In-flight claim bits keep the heap live.
- Never-bound freers enqueue each successful claim in `Allocator::free_cross_heap` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free composition stays on `HeapSlot::free` / `flush`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Sticky-empty refill prefers local/OS `acquire_run` before inbox flush so lock-free remote fan-in does not force the owner onto accept every refill.
- `SlotState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **publisher lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- `HeapSlot` metadata is one `UnsafeCell<SlotHeap>` (`heap_mut()`): Active TLS owner or directory-locked Draining/Free only. No whole-slot `&mut` after publication.
- `HeapDirectory` publishes stable slot pointers per index once; lookup is lock-free. Lifecycle mutex is private inside the directory facade.
