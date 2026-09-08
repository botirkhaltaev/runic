# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `HeapInner`, `AllocatorCtx`, and re-exports.
- `heaps.rs`: `Heaps` (`RwLock<Arena<Heap>>` + Free-heap freelist).
- `state.rs`: `HeapMode`, `HeapState`, `Lease` (`store` is module-private to reactivate / bump).
- `inbox.rs`: `Inbox` / `InboxLink`.
- `thread.rs`: `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run`, heap-owned maps, `RunHeap` with `Arena<Run>`, `RunCache`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `retire` / `enqueue` / `free` / `flush` / `reclaim` | hold arena lock across flush/accept |
| `&Heap` (shared) | `enqueue`, mode / active queries | body mutation, expose `&HeapState` |
| `ThreadHeap` | sole Active body path (`try_inner` + `AllocatorCtx`) | be bypassed via `&Heap` from allocator / tests |
| `AllocatorCtx` | pass `PageMap` + `Heaps` into Heap / ThreadHeap / Heaps methods | contain a mutex guard |

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active: lease before a new `try_queue`) or `Heaps::{enqueue,free,flush}` (Draining). Coalescing is by owner. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the space tail. Owner `Run::free` is locate + pointer push; owner DF is undefined. Extents use byte `Claimed`.
- Inbox is a Treiber stack of run/extent nodes. `drain` is a single-pass walk.
- Draining reclaim observes live ownership via `RunHeap` ∨ `ExtentHeap` (`has_live`). In-flight claim bits keep the heap live. `Heap::reclaim` returns a Free heap to the table freelist.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote`. Bound producers coalesce by run/extent. `ThreadFreeError::Remote` carries the `PageOwner` `free_slow` already looked up.
- Owner free: `Run::free` (lock-free); `push_available` only on `was_full` (idempotent). `unbind` returns non-full current runs. Draining late free uses `Heap::free`. Domain ops are `free` / `claim` / `accept`. Failures after claim abort.
- Current-run empty: `extend`; accept inbox if nonempty; then local/OS `acquire_run`. Unbound: `bind` then `flush` then alloc. Hit: current pop / `RunCache` `Run::free`. Inbox `flush` is remote `accept`.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight lease count for Active enqueue admits. Inbox depth stays live via claim bits / `has_live`.
- `Heaps` is `RwLock<Arena<Heap>>`. `get` indexes then returns `&Heap` (occupied slots never move). Write lock covers acquire/reuse only. Free heaps sit on an intrusive index freelist. Fail only when the OS will not map more.
- `THREAD_HEAP` has no destructor. `UnbindGuard` (touched in `bind`) retires the heap on thread exit.

## Current run (hit)

A small block is on exactly one of: user, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `class_for` → `current[class]` → `Run::allocate` (pop) | `extend`, claim bits, locks, atomics, acquire, flush |
| **owner free** | `RunCache` → `current[class]` payload range → `PageMap`; `Run::free` (locate + push); `push_available` only on `was_full` | claim bits, `extend`, locks, atomics |

`current[class]` is a hint, not ownership. Available list is the reservoir; a run may be both current and listed. Frees never touch `current`. Interior pointers abort on `locate`. Owner DF is undefined.

Owner free / realloc use `lookup` (`RunCache` → `current[class]` → `PageMap`). The cache stores only runs whose `HeapId` matches this TLS. `ExtentCache` is heap-level mapping reuse, not a TLS free probe.
