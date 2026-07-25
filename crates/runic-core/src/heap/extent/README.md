# heap/extent

Extent metadata owns dedicated large allocations.

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, and resize-in-place rules.
- `cache.rs`: bounded retained-mapping cache (`ExtentPolicy::{Drop, Keep}`, exact-length reuse only).
- `heap.rs`: dedicated allocation via `ExtentInit`, `Arena<Extent>`, page-map publication, and the shared local/remote retire path.

## Same-thread fast path

`ThreadHeap::alloc_extent` / `ThreadHeap::free_extent` call `Heap::allocate_extent` / `Heap::free_extent_owner` on the bound heap without taking the table mutex. Extents have no sticky TLS slot cache (unlike runs) because `ExtentCache` already owns mapping reuse. `Allocator::alloc`, `alloc_zeroed`, and `dealloc` try this path first and fall back to `bind` + locked heap work only on TLS miss or cross-heap pointers.

## Invariants

- An extent owns one mapping dedicated to one returned allocation and stores a `HeapId`.
- Frees must use the exact returned pointer, not an interior pointer.
- Remote frees claim pending before enqueue; only the owning heap (or draining freer) completes the free.
- `ExtentHeap::free` and `ExtentHeap::complete_remote_free` each validate through their own path (local CAS vs. remote-pending check) and then retire through one shared `retire` method: unpublish, remove the arena slot, offer the mapping to `ExtentCache`.
- Live large ownership for reclaim is occupied arena presence (`ExtentHeap::has_live_allocations`).
- Page-map entries must be removed before extent metadata is removed.
- `ExtentInit::Zeroed` memsets only on cache hits (size from `LayoutSpec`); fresh anonymous mappings skip that memset.
- `ExtentCache` retention must stay within configured slot and byte budgets; `Keep` never evicts an already-retained mapping to admit a new one, and reuse is always exact-length.
