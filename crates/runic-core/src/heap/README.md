# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `LockedHeap`, and re-exports.
- `heaps.rs`: `Heaps` (index/publish).
- `state.rs`: `HeapMode`, `HeapState`, `Lease` (`store` is module-private to reactivate / bump).
- `inbox.rs`: `Inbox` / `InboxLink`.
- `thread.rs`: `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run`, `RunHeap` with `Arena<Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `retire` / `lock` | flush, accept, mutate `RunHeap` / `ExtentHeap` |
| `&Heap` (shared) | `enqueue`, mode / active queries | body mutation, `reclaim`, expose `&HeapState` |
| `ThreadHeap` | sole Active body path | be bypassed via `&Heap` from allocator / tests |
| `LockedHeap` | sole Draining body + reclaim on Drop | exist outside `Heaps::lock` |

## Invariants

- Every `Run` and `Extent` stores a `HeapId`; there is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active: lease **before** new `try_queue`, then link) or `Heaps::lock` → `LockedHeap` (exclusive late free / push). Coalescing is by owner. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the mapping tail (`issued` + `try_set`). Owner `Run::free` is `locate` + pointer push; owner DF is undefined. Extents use byte `Claimed`.
- `Inbox::push` / `link` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap` ∨ `ExtentHeap` (`has_live`). In-flight claim bits keep the heap live. Only `LockedHeap` Drop may reclaim.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free composition stays on private `Heap` body helpers invoked only from `ThreadHeap` / `LockedHeap`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Current-run empty: `extend` then local/OS `acquire_run` first; inbox flush only if that misses, then retry. Unbound cold path: `alloc_after_bind` / `alloc_extent_after_bind`. Hit: current pop / `Heap::free_run`. Inbox `flush` is remote `accept`.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- `Heaps` publishes stable heap pointers per index once; `get` is lock-free. Arena mutex covers claim/reuse only — never flush/accept.
- `THREAD_HEAP` has no destructor. `UnbindGuard` (touched in `bind`) retires the heap on thread exit.

## Current run (hit)

A small block is on exactly one of: user, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `matches` → `current[class]` → `Run::allocate` (pop) | `extend`, ClaimBits, locks, atomics, acquire, flush |
| **owner free** | page-cache (own-heap runs only) → `Heap::free_run` (`locate` + `live--` + push) | ClaimBits, `extend`, locks, atomics |

`current[class]` is a hint, not ownership. Available list is the reservoir; a run may be both current and listed. Frees never touch `current`. Interior pointers abort on `locate` (hit). Owner DF is undefined.

`lookup` + the page cache stay on owner free. Cache stores only runs whose `HeapId` matches this TLS.

#129 closeout (this host, `aa3a83a`): churn/64 is 43.6 vs snmalloc 27.4 (1.6×).
This pass vs `c1ecdeb`: churn/64 **35.4**, `owner_free` **22.2**, `freelist` **18.5**.
Isolated owner_free / freelist are the hit (`locate` / pop).
`#135` RSEQ per-CPU: 65.3 vs 43.6, reverted.
