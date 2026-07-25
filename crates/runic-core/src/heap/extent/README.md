# heap/extent

Extent metadata owns dedicated large allocations.

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, reuse, and resize-in-place rules.
- `cache.rs`: bounded index of published Free extents (`ExtentPolicy::{Drop, Keep}`, exact-length reuse only).
- `heap.rs`: dedicated allocation via `ExtentInit`, `Arena<Extent>`, page-map publication, and the shared local/remote retire path.

## Same-thread fast path

`ThreadHeap::alloc_extent` / `ThreadHeap::free_extent` call `Heap::allocate_extent` / `Heap::free_extent_owner` on the bound heap without taking the table mutex. Extents have no sticky TLS slot cache (unlike runs) because `ExtentCache` already owns reuse. `Allocator::alloc`, `alloc_zeroed`, and `dealloc` try this path first and fall back to `bind` + locked heap work only on TLS miss or cross-heap pointers.

## Invariants

- An extent owns one mapping dedicated to one returned allocation and stores a `HeapId`.
- Frees must use the exact returned pointer, not an interior pointer.
- Remote frees claim pending before enqueue; only the owning heap (or draining freer) completes the free.
- `ExtentHeap::free` and `ExtentHeap::complete_remote_free` each validate through their own path (local CAS vs. remote-pending check) and then retire through one shared `retire` method.
- **Published-while-cached:** Keep retention leaves the arena entry and page-map stamp in place; the cache stores `NonNull<Extent>`. Cache-hit allocate calls `Extent::reuse` and does not republish. True release (Drop policy / over budget) unpublishes before removing metadata.
- Live large ownership for reclaim is Allocated/RemotePending (`ExtentHeap::has_live_extents`); cached Free extents do not block reclaim.
- `ExtentInit::Zeroed` memsets only on cache hits (size from `LayoutSpec`); fresh anonymous mappings skip that memset.
- `ExtentCache` retention must stay within configured slot and byte budgets; `Keep` never evicts an already-retained extent to admit a new one, and reuse is always exact mapping length.
