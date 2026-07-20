# heap/extent

Extent metadata owns dedicated large allocations.

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, and resize-in-place rules.
- `cache.rs`: bounded retained-mapping cache and extent release policies.
- `heap.rs`: dedicated allocation via `ExtentInit`, `Arena<Extent>`, page-map publication, free, and mapping reuse.

## Invariants

- An extent owns one mapping dedicated to one returned allocation and stores a `HeapId`.
- Frees must use the exact returned pointer, not an interior pointer.
- Remote frees claim pending before enqueue; only the owning heap (or draining freer) completes the free.
- Page-map entries must be removed before extent metadata is removed.
- `ExtentInit::Zeroed` memsets only on cache hits (size from `LayoutSpec`); fresh anonymous mappings skip that memset.
- `ExtentCache` retention must stay within configured slot and byte budgets.
