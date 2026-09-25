# heap/extent

Extent metadata owns dedicated large allocations. Retention:
[ARCHITECTURE.md](../../../../../ARCHITECTURE.md).

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, reuse, and resize-in-place rules.
- `config.rs`: `ExtentConfig` / `ExtentPolicy::{Keep, Discard, Unmap}` and `Budget`.
- `cache.rs`: intrusive `head` list of published Free extents (`ExtentPolicy::{Keep, Discard, Unmap}`, exact-length reuse only). Discard `madvise`s on insert.
- `heap.rs`: dedicated allocation via `ExtentInit`, `Arena<Extent>`, `Hints` on payload maps, page-map publication, and `cache_or_unmap` / `unmap`.

## Same-thread path

`ThreadHeaps::alloc_extent` / `free_extent` call `Heap` on an owned TLS heap. Large reuse is `ExtentCache` (exact mapping length). Unbound cold path is `Allocator::bind_alloc`.

## Invariants

- An extent owns at most one mapping dedicated to one returned allocation and stores its process-lifetime owning `&Heap`; `heap().id()` derives the current generation. Its arena slot is immortal; unmap drops only the mapping and reuses the slot later.
- Frees must use the exact returned pointer, not an interior pointer. Owner
  double-free is undefined. Remote `claim` / `accept` still fail closed.
- Remote frees `claim` then enqueue; the owning heap completes with `accept` (`Claimed → Free`) before shared `cache_or_unmap`.
- **Published-while-cached:** Keep and Discard leave the arena entry and page-map stamp in place; the cache is an intrusive `head` list of `ExtentId` values into the owning arena. Cache-hit allocate calls `Extent::reuse(init)` and does not re-publish the mapping. True release (Unmap policy / over budget) calls `unmap`, which unpublishes and drops the mapping while retaining the immortal slot. Discard then `madvise(MADV_DONTNEED)`s the mapping.
- Live large ownership increments/decrements the owning `Heap` atomic; `ExtentHeap::has_live` confirms by scanning Allocated/Claimed slots. Cached Free extents do not block reclaim.
- `resize_in_place` stays an extent layout (`class_for` is `None`). A size-class spec returns false so later small dealloc probes `Run::header_of` only on runs.
- `ExtentInit::Zeroed` on dirty Keep reuse: `MADV_DONTNEED` when size ≥ 64 KiB, else memset. That allocate-time discard does not set `clean`; only a successful Discard-insert does. Fresh maps and clean cache hits skip the memset. Uninit never discards.
- `ExtentCache` retention must stay within configured slot and byte budgets; `Keep` never evicts an already-retained extent to admit a new one, and reuse is always exact mapping length. Default budget is 64 slots / 64 MiB.
