# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `HeapInner`, `HeapCtx`, `HeapsCtx`, and re-exports.
- `heaps.rs`: `Heaps` (`RwLock<Arena<Heap>>` + Free-heap freelist).
- `state.rs`: `HeapMode`, `HeapState`, `Lease` (`store` is module-private to reactivate / bump).
- `inbox.rs`: `Inbox` / `InboxLink`.
- `thread.rs`: `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`, `RunCache`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `retire` / `enqueue` / `free` / `flush` / `reclaim` | hold arena lock across flush/accept |
| `&Heap` (shared) | `enqueue`, mode / active queries | body mutation, expose `&HeapState` |
| `ThreadHeap` | sole Active body path (`try_inner` + `HeapCtx`) | be bypassed via `&Heap` from allocator / tests |
| `HeapCtx` / `HeapsCtx` | pass `PageMap` / `Heaps` into Heap methods | contain a mutex guard |

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active: lease **before** new `try_queue`, then link) or `Heaps::{enqueue,free,flush}` (Draining). Coalescing is by owner. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the mapping tail (`issued` + `try_set`). Owner `Run::free` is `locate` + pointer push; owner DF is undefined. Extents use byte `Claimed`.
- `Inbox::push` / `link` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap` ∨ `ExtentHeap` (`has_live`). In-flight claim bits keep the heap live. `Heap::reclaim` (via `HeapsCtx`) returns a Free heap to the table freelist.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free: `Run::free` (lock-free); `push_available` only on `was_full`. Draining late free uses `Heap::free` (`&mut HeapInner` + ctx). Domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Current-run empty: `extend`; accept inbox if nonempty (same as `alloc_extent`); then local/OS `acquire_run`. Unbound cold path: `alloc_after_bind` / `alloc_extent_after_bind`. Hit: current pop / `RunCache` `Run::free`. Inbox `flush` is remote `accept`.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- `Heaps` is `RwLock<Arena<Heap>>`. `get` takes a read lock only to index, then returns `&Heap` (occupied slots never move). Write lock covers acquire/reuse only — never flush/accept. Free heaps sit on an intrusive index freelist (occupied `Free` slots, not Arena vacant slots). Fail only when the OS will not map more.
- `THREAD_HEAP` has no destructor. `UnbindGuard` (touched in `bind`) retires the heap on thread exit.

## Current run (hit)

A small block is on exactly one of: user, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `matches` → `current[class]` → `Run::allocate` (pop) | `extend`, ClaimBits, locks, atomics, acquire, flush |
| **owner free** | `RunCache::hit` (`base == usize::MAX` empty; probe `ptr - base < RUN_SIZE`) → `Run::free` (span + reciprocal divisibility `locate` + `live--` + push); `push_available` only on `was_full` | ClaimBits, `extend`, locks, atomics, jump table |

`current[class]` is a hint, not ownership. Available list is the reservoir; a run may be both current and listed. Frees never touch `current`. Interior pointers abort on `locate` (hit). Owner DF is undefined.

`lookup` + `RunCache` stay on owner free. The cache stores only runs whose `HeapId` matches this TLS. `ExtentCache` is heap-level mapping reuse, not a TLS free probe.

#129 closeout (this host, `aa3a83a`): churn/64 is 43.6 vs snmalloc 27.4 (1.6×).
This pass vs post-#142 `82dd8b5`: `owner_free` **11.1** (was 15.3; sn 11.5), churn **30.2** (was 31.6), `freelist` **20.4** (flat guard).
`free_slow` on owner_free/64: 1.96% → <0.05%. Locate is span + reciprocal divisibility (no jump table).
`owner_free/4096` 30.7; `recycled_churn/64/live:256` 57.2; `sh6bench/runic/1` 9488 (no post142 pair).
Locate diet vs `75bb578` was owner_free 16.9 / freelist 18.5 / churn 37.4.
`#135` RSEQ per-CPU: 65.3 vs 43.6, retired (superset of the TLS hit on pinned churn).
O(1) TLS steal: freelist 18.5 → 22.9, reverted.
