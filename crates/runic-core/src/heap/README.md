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
- Run remote admission is a private claim bitmap in the mapping tail. Owner `Run::free` stores Free then rechecks the claim bit (`Claimed` if the bit won; `accept` publishes). Extents use byte `Claimed`.
- `Inbox::push` / `link` is a Treiber CAS loop on run/extent nodes: link `next` to old head, then CAS `head`. `drain` is a single-pass null-terminated walk.
- Draining reclaim observes live ownership via `RunHeap` ∨ `ExtentHeap` (`has_live`). In-flight claim bits keep the heap live. Only `LockedHeap` Drop may reclaim.
- Never-bound freers enqueue each successful claim in `Allocator::free_remote` (no TLS batch; no stranded claims). Bound producers coalesce by run/extent, not by thread batch.
- Owner free composition stays on private `Heap` body helpers invoked only from `ThreadHeap` / `LockedHeap`; domain ops are `free` / `claim` / `accept` on `Run`/`Extent`. Failures after claim abort (no rollback).
- Magazine-empty refill: local/OS `acquire_run` first; inbox flush only if that misses, then retry. Batch `Run::allocate` into the magazine (stop at watermark−1). Unbound cold path: `alloc_after_bind` / `alloc_extent_after_bind` (one flush-then-alloc; no `*_fresh`). Hit: lockless pop/push only (below). Magazine drain is `take` then `Heap::free` (body only; not inbox `flush`).
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- `Heaps` publishes stable heap pointers per index once; `get` is lock-free. Arena mutex covers claim/reuse only — never flush/accept.

## Magazine (hit vs take)

A small block is on exactly one of: user, magazine, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `matches` (pointer eq) + magazine `pop` | `Run::allocate`, ClaimBits, BlockStates, `live`, locks, atomics |
| **owner free** | `lookup` + owner (`matches` + `HeapId`) + magazine `push` | `locate`, ClaimBits, BlockStates, `push_free`, `live--`, locks, atomics |

Pop/push are `Cell` loads/stores and an intrusive payload `usize` link. No mutex, no CAS, no `Atomic*` on magazine links, no `Run` on that path.

`Run::free` (Free + claim recheck) and freelist publish happen only when the magazine is **taken** (count ≥ watermark, or unbind). Isolated `owner_free_only` / `freelist_allocate_only` benches still pay that `take`/`allocate` work; they are not the hit. Do not raise the watermark to hide them. Cost for this change is churn (alloc and free meet before `take`). Inbox `flush` is a different operation (remote `accept`).

**Linearization (take-closed DF).** Remote `claim` still handshakes against Free. A magazine-resident block is still Clear, so `claim` can win until `take`. That free is exact-once on the remote path. `Run::free` returns `Claimed` if claim already won (`accept` publishes). Owner double-free (two pushes of the same pointer) is detected at `take` (second `free` sees Free), not on the second push. Unbind takes every class so no magazine object is stranded.

`lookup` + the page cache stay on owner free (dropping identity is a later issue). Extents have no magazine.
