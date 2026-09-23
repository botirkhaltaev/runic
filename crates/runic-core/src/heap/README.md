# heap

Owner-local heap frontend: runs for small size classes, extents for dedicated large allocations, and Heaps / thread binding.

## Layout

- `error.rs`: `HeapError` at the heap edge (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`, …) + `From<RunError>` / `From<ExtentError>`.
- `id.rs`: `HeapId` (heap index + generation). Arena / `*Id` indices are `u32`; `usize` only at array/pointer edges.
- `mod.rs`: `Heap`, `HeapInner`, `AllocatorCtx`, and re-exports.
- `heaps.rs`: `Heaps` (`Arena<Heap>` + Free-heap freelist).
- `state.rs`: `HeapMode`, `HeapState`, `Lease` (`store` is module-private to reactivate / bump).
- `inbox.rs`: generic `Inbox<'a, T>` / `Node` / `Link<T>` / `Chain<'a, T>`; queueing stores the node borrow, so draining returns that lifetime rather than extending an inbox borrow.
- `thread.rs`: `ThreadHeaps` / `ThreadHeap`.
- `run/`: size-classed fixed-block runs (`Run` in-page header, heap-owned maps, `RunHeap` with `Arena<&'static Run>`).
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `unbind` / `free` / `flush` | hold arena grow lock across flush/accept |
| `&Heap` (shared) | `id`, `active_id`, `enqueue`, mode | body mutation, expose `&HeapState` |
| `ThreadHeaps` | sole Active body path (`require_inner` + `AllocatorCtx`) | be bypassed via `&Heap` from allocator / tests |
| `AllocatorCtx` | pass `PageMap` + `Heaps` into Heap / ThreadHeaps / Heaps methods | contain a mutex guard |

## Invariants

- Every `Run` and `Extent` stores its process-lifetime owning `&Heap`; `heap().id()` derives the current generation. There is no root/central ownership heap. `Heap` owns lifecycle, inboxes, and run/extent metadata (`RunHeap` / `ExtentHeap`).
- `Heap`, `Run`, and `Extent` implement identity `Eq`; protocol code compares entities directly. Raw pointer equality stays inside those trait implementations.
- Small allocations are owned by a heap's runs; large allocations by that heap's extents.
- Cross-thread frees: `claim` → `Heap::enqueue` (Active: lease is the admit) or `Heaps::{free,flush}` (Draining). `Heaps::admit` / `flush` take `owner: Option` — `Some` admits on `owner.heap()` and queues that claim under the same Inner lock; `None` `get`s (unbind). A claimed free retries when close/adopt changes the mode; if reclamation advances the generation, the old owner necessarily accepted that claim. The first remote freer into a Draining heap may `adopt` it (`Draining` → `Active`); adoption locks `HeapInner` before its lifecycle CAS and keeps that guard for the first flush, so reclaim cannot overwrite a winner. Later frees from that thread are owner-local. TLS `ThreadHeaps` holds two equal `ThreadHeap::{Vacant, Active}` values. `Active` is `&Heap` plus the captured `HeapId`. Bind and adopt take the first vacant slot; alloc uses the first active heap. A third Draining heap stays on `Heaps::free` until a slot is unbound (two-heap cap lost on `channel_pipeline`). Coalescing is by owner. Owner `flush` drains via `accept`.
- Run remote admission is a private claim bitmap in the space tail. Owner `Run::free` is locate + pointer push; owner DF is undefined. Extents use byte `Claimed`.
- Inbox is a Treiber stack of run/extent nodes. `drain` is a single-pass walk.
- Draining reclaim first observes `Heap` run/extent live atomics, then confirms with arena scans. In-flight claim bits keep the heap live. `Heap::reclaim` returns a Free heap to the table freelist.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote`. Bound producers coalesce by run/extent. `ThreadFreeError::Remote` carries the `PageOwner` `free_slow` already looked up.
- Owner free hit: `Run::free` (lock-free locate + push). Hit ignores the `RunFree` outcome and Discard. `push_available` / `discard` (guarded by `is_discardable`) are miss / slow / unbind. Miss / extra-local is `ThreadHeaps::free_run` after `lookup`. `idle` is a query over every slot; its caller returns that heap's current runs then unbinds the `ThreadHeap` when another heap is still attached. Draining late free uses `Heaps::free` when adopt does not win. Domain ops are `free` / `claim` / `accept`. Invalid domain state after lifecycle retries aborts.
- Current-run empty: `extend`; accept inbox if nonempty; then local/OS `acquire_run`. Unbound: `bind` then `flush` then alloc. Hit: current pop / `Run::free`. Inbox `flush` is remote `accept`. `lookup` is miss / realloc.
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining` / terminal `Retired`), and in-flight lease count for Active enqueue admits. `adopt` is Draining→Active (leases unchanged). Inbox depth stays live via claim bits / `has_live`.
- `Heaps` is `Arena<Heap>`. `get` is lock-free `Arena` then `Heap::matches` (slot + generation). Arena grow covers mapping ownership and bump insert only. Free heaps sit on an intrusive index freelist. Reclaim uses a lifecycle CAS, never an unconditional store. Fail only when the OS will not map more, or the arena is full.
- `THREAD_HEAPS` is a `#[thread_local]` `!Drop` value (`%fs` load). Each active `ThreadHeap` captures `&Heap` plus the generation token at bind/adopt so unbind cannot close a later incarnation. Default builds use the fast Rust TLS `UnbindGuard`. The `c-abi` feature uses `UnbindHook`, a once-per-thread `pthread` key; glibc registers Rust TLS destructors through `__cxa_thread_atexit_impl`, which `calloc`s and re-enters `bind` under `LD_PRELOAD`.

## Current run (hit)

A small block is on exactly one of: user, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `class_for` → `current[class]` → `Run::allocate` (pop) | `extend`, claim bits, locks, acquire, flush. Heap live counter only on 0→1 |
| **owner free** | `current[class]` → `Run::free` (one `locate` + push); `OutOfRange` is miss | `push_available`, Discard, claim bits, `extend`, locks, `PageMap`. Heap live counter only on 1→0 |

`current[class]` is a hint, not ownership. Available list is the reservoir; a run may be both current and listed. Frees never touch `current`. Interior pointers abort on `locate`. Owner DF is undefined.

Miss / realloc use `lookup` (`header_of` for a small layout, else `PageMap`). Pointer-only C `free` / `resize` use `PageMap` only: `header_of` can fault on an extent. C `free` still tries the current-run hit after PageMap names a run. `ExtentCache` is heap-level mapping reuse, not a TLS free probe.
