# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `HeapInner`, `AllocatorCtx`, and re-exports.
- `heaps.rs`: `Heaps` (`Arena<Heap>` + Free-heap freelist).
- `state.rs`: `HeapMode`, `HeapState`, `Lease` (`store` is module-private to reactivate / bump).
- `inbox.rs`: `Inbox` / `InboxLink`.
- `thread.rs`: `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run` in-page header, heap-owned maps, `RunHeap` with `Arena<NonNull<Run>>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `retire` / `enqueue` / `free` / `flush` / `reclaim` | hold arena grow lock across flush/accept |
| `&Heap` (shared) | `id`, `enqueue`, mode / active queries | body mutation, expose `&HeapState` |
| `ThreadHeap` | sole Active body path (`require_inner` + `AllocatorCtx`) | be bypassed via `&Heap` from allocator / tests |
| `AllocatorCtx` | pass `PageMap` + `Heaps` into Heap / ThreadHeap / Heaps methods | contain a mutex guard |

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active: lease before a new `try_queue`) or `Heaps::{enqueue,free,flush}` (Draining). The first remote freer into a Draining heap may `adopt` it (`Draining` → `Active`); later frees from that thread are owner-local. One adopted heap besides the bound heap; a second Draining heap stays on `Heaps::free` until unbind (multi-slot adopt lost on `channel_pipeline`). `alloc` never uses it. Coalescing is by owner. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the space tail. Owner `Run::release` is locate + pointer push; owner DF is undefined. Extents use byte `Claimed`.
- Inbox is a Treiber stack of run/extent nodes. `drain` is a single-pass walk.
- Draining reclaim observes live ownership via `Heap::has_live` (`run_live` / `extent_live`). In-flight claim bits keep the heap live. `Heap::reclaim` returns a Free heap to the table freelist.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote`. Bound producers coalesce by run/extent. `ThreadFreeError::Remote` carries the `PageOwner` `free_slow` already looked up.
- Owner free hit: `Run::release` (lock-free locate + push). `push_available` is miss / slow / unbind (`Run::free` still reports `was_full`). Miss / adopt-local is `ThreadHeap::free_owner` after `lookup`. `unbind` / `retire_adopted` retire the adopted heap; `retire_if_idle` only when that heap emptied. Draining late free uses `HeapInner::free` via `Heaps::free` when adopt does not win. Domain ops are `free` / `claim` / `accept`. Failures after claim abort.
- Current-run empty: `extend`; accept inbox if nonempty; then local/OS `acquire_run`. Unbound: `bind` then `flush` then alloc. Hit: current pop / `Run::release`. Inbox `flush` is remote `accept`. `lookup` is miss / realloc.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight lease count for Active enqueue admits. `adopt` is Draining→Active (leases unchanged). Inbox depth stays live via claim bits / `has_live`.
- `Heaps` is `Arena<Heap>`. `get` is lock-free `Arena` then `state.matches`. Arena grow covers mapping ownership and bump insert only. Free heaps sit on an intrusive index freelist. Fail only when the OS will not map more, or the arena is full.
- `THREAD_HEAP` is a `#[thread_local]` `!Drop` value (`%fs` load). `UnbindGuard` is the only `LocalKey` (touched in `bind`; `Drop` retires the heap).

## Current run (hit)

A small block is on exactly one of: user, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `class_for` → `current[class]` → `Run::allocate` (pop) | `extend`, claim bits, locks, atomics, acquire, flush |
| **owner free** | `current[class]` → `Run::release` (one `locate` + push); `OutOfRange` is miss | `push_available`, claim bits, `extend`, locks, `PageMap` |

`current[class]` is a hint, not ownership. Available list is the reservoir; a run may be both current and listed. Frees never touch `current`. Interior pointers abort on `locate`. Owner DF is undefined.

Miss / realloc use `lookup` (`header_of` for small, else `PageMap`). `ExtentCache` is heap-level mapping reuse, not a TLS free probe.
