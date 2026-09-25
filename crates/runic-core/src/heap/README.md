# heap

Owner-local heap frontend. See [Architecture](../../../../ARCHITECTURE.md) for
process-wide flows.

## Layout

- `error.rs`: `HeapError` and conversions from run/extent errors.
- `id.rs`: `HeapId` (heap index plus generation).
- `mod.rs`: `Heap`, `HeapInner`, `AllocatorCtx`, and re-exports.
- `heaps.rs`: `Heaps` (`Arena<Heap>` + Free-heap freelist).
- `state.rs`: `HeapMode`, `HeapState`, `Lease`.
- `list.rs`: intrusive lock-free `List` (push onto the head, `drain` takes all).
- `inbox.rs`: coalescing `Inbox`, `Node`, and `Link` over `List`.
- `thread.rs`: `ThreadHeaps` / `ThreadHeap`.
- `run/`: fixed-block runs and heap-owned run maps.
- `extent/`: dedicated mappings (`Extent`, `ExtentHeap` with `Arena<Extent>`, `ExtentCache`).

## Capabilities

| Entity | May do | Must not |
|--------|--------|----------|
| `Heaps` | `acquire` / `get` / `unbind` / `free` / `flush` | hold arena grow lock across flush / accept |
| `&Heap` (shared) | `id`, `active_id`, `enqueue`, mode | body mutation, expose `&HeapState` |
| `ThreadHeaps` | Active mutation (`require_inner` + `AllocatorCtx`) | bypass ownership through `&Heap` |
| `AllocatorCtx` | carry `PageMap` + `Heaps` | contain a mutex guard |

## Invariants

- Every `Run` and `Extent` stores its process-lifetime `&Heap`; there is no
  root heap.
- `Heap`, `Run`, and `Extent` implement identity equality. Raw pointer equality
  stays inside those implementations.
- Active mutation goes through `ThreadHeaps`; Draining mutation goes through
  `Heaps`.
- Remote free is `claim`, enqueue, then `accept`. Runs use a claim bitmap;
  extents use a `Claimed` byte. Inbox nodes are intrusive and coalesce by owner.
- Reclaim checks run/extent live atomics, confirms with arena scans, then uses a
  lifecycle CAS to return the slot to the Free list.
- `HeapState` packs generation, mode, and Active lease count. Each TLS slot
  captures its generation so unbind cannot close a later incarnation.
- The arena grow lock covers mapping and insertion only.
- `THREAD_HEAPS` is `#[thread_local]` and `!Drop`. Default builds register
  `UnbindGuard`; `c-abi` registers a pthread `UnbindHook`.

## Current run (hit)

A small block is held by the user, the run freelist, or a remote claim.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `class_for` then `current[class]` then `Run::allocate` | `extend`, claim bits, locks, acquire, flush |
| **owner free** | `current[class]` then `Run::free`; `OutOfRange` is miss | `push_available`, Discard, claim bits, locks, `PageMap` |

`current[class]` is a hint, not ownership. A run may also be listed as
available. Free does not change `current`. Interior pointers abort on `locate`;
owner double-free is undefined.

Miss and realloc use `lookup` (`header_of` for a small layout, otherwise
`PageMap`). Pointer-only C `free` / `resize` use `PageMap` because `header_of`
can fault on an extent. After PageMap names a run, C `free` still tries the
current-run hit.
