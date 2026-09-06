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
- Magazine-empty refill: local/OS `acquire_run` first; inbox flush only if that misses, then retry. Batch `Run::allocate` into the magazine (stop at watermark−1). Unbound cold path: `alloc_after_bind` / `alloc_extent_after_bind` (one flush-then-alloc; no `*_fresh`). Hit: lockless pop/push only (below). Magazine drain is `take` then `Heap::free` (body only; not inbox `flush`).
- `HeapState` packs generation, mode (`Free` / `Active` / `Draining`), retired, and in-flight **lease** count for Active **enqueue** admits only (not inbox depth — that stays live via claim bits / `has_live`).
- `Heaps` publishes stable heap pointers per index once; `get` is lock-free. Arena mutex covers claim/reuse only — never flush/accept.

## Magazine (hit vs take)

A small block is on exactly one of: user, magazine, run freelist, or remote-claimed.

| Hit | Work | Not on the hit |
|-----|------|----------------|
| **alloc** | `class` → `matches` → magazine `pop` | `Run::allocate`, ClaimBits, `live`, locks, atomics, refill |
| **owner free** | page-cache → `HeapId` → magazine `push` | `locate`, ClaimBits, `push_free`, `live--`, locks, atomics, take |

Pop/push are `Cell` loads/stores and an intrusive payload `usize` link. No mutex, no CAS, no `Atomic*` on magazine links, no `Run` body on that path. Cold outlines (`refill`, take, extent, bind) are `#[cold] #[inline(never)]`.

`Run::free` (locate + pointer push) and freelist publish happen only when the magazine is **taken** (count ≥ watermark, or unbind). Isolated `owner_free_only` / `freelist_allocate_only` benches still pay that `take`/`allocate` work; they are not the hit. Do not raise the watermark to hide them. Inbox `flush` is a different operation (remote `accept`).

**Remote admission.** `claim` is `issued` + `try_set` (duplicate remote claim fails closed). `accept` publishes. Owner double-free and realloc-after-free are undefined. Interior / foreign pointers still abort. Unbind takes every class so no magazine object is stranded.

`lookup` + the page cache stay on owner free. Post-magazine Where on this host
(`5946084`): identity is page# + cache compare (~10% of inlined `dealloc`, ~4% of
churn). `PageMap::get` is ~0% on same-run churn. Isolated `owner_free` is
`take` / `Run::free`, not lookup. #126 skipped — not a ≥5% lever.

#128 skipped: grouping a taken magazine by run (one `RunState` / available-list
transition) vs `5946084` Cost: `owner_free_only` 61.9 → 94.7 cyc/elem (+53%),
`freelist_allocate_only` −4% (under gate), `single_size_churn` 43.7 → 56.3
(+29%). Per-block `Heap::free` on take stays. Extents have no magazine.

#129 closeout (this host, `aa3a83a`): churn/64 is 43.6 vs snmalloc 27.4 (1.6×).
This pass (hit diet + take/refill diet): churn/64 **41.3** (P1, ≤41.4 gate),
then **43.0** after P2. `owner_free` 62→72 (take still locate + magazine drain;
BlockStates deletion did not close the 4.6× gap). `freelist` 42.4→37.1.
`#135` RSEQ per-CPU: 65.3 vs 43.6, reverted. Watermark stays 32.
