# heap/extent

Extent metadata owns dedicated large allocations.

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, reuse, and resize-in-place rules.
- `cache.rs`: bounded index of published Free extents (`ExtentPolicy::{Drop, Keep}`, exact-length reuse only).
- `heap.rs`: dedicated allocation via `ExtentInit`, `Arena<Extent>`, page-map publication, and `cache_or_unmap` / `unmap`.

## Same-thread fast path

`ThreadHeap::alloc_extent` / `ThreadHeap::free_extent` call `HeapSlot` on the bound heap without taking the directory lifecycle mutex. Extents have no sticky TLS slot cache (unlike runs) because `ExtentCache` already owns reuse. `Allocator::alloc` / `alloc_zeroed` try the TLS path first and fall back to `alloc_unbound` when unbound; `dealloc` uses `free_cross_heap` for cross-heap pointers.

## Invariants

- An extent owns one mapping dedicated to one returned allocation and stores a `HeapId`.
- Frees must use the exact returned pointer, not an interior pointer; `Extent` validates before any state CAS.
- Remote frees `claim` then enqueue; the owning heap completes with `accept` (`Claimed → Free`) before shared `cache_or_unmap`.
- **Published-while-cached:** Keep retention leaves the arena entry and page-map stamp in place; the cache stores `NonNull<Extent>`. Cache-hit allocate calls `Extent::reuse` and does not re-publish the mapping. True release (Drop policy / over budget) calls `unmap`, which unpublishes before removing metadata.
- Live large ownership for reclaim is `Extent::is_live` (Allocated/Claimed), aggregated by `ExtentHeap::has_live`; cached Free extents do not block reclaim.
- `ExtentInit::Zeroed` memsets only on cache hits (size from `LayoutSpec`); fresh anonymous mappings skip that memset.
- `ExtentCache` retention must stay within configured slot and byte budgets; `Keep` never evicts an already-retained extent to admit a new one, and reuse is always exact mapping length.
