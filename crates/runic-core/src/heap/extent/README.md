# heap/extent

Extent metadata owns dedicated large allocations.

## Files

- `mod.rs`: `Extent`, `ExtentId`, exact-pointer checks, and resize-in-place rules.
- `arena.rs`: out-of-line extent metadata storage and reservations.
- `cache.rs`: bounded retained-mapping cache and extent release policies.
- `heap.rs`: dedicated allocation policy, page-map publication, free, and mapping reuse.

## Invariants

- An extent owns one mapping dedicated to one returned allocation.
- Frees must use the exact returned pointer, not an interior pointer.
- Page-map entries must be removed before extent metadata is removed.
- Reused mappings need zeroing before they are returned to callers.
- `ExtentCache` retention must stay within configured slot and byte budgets.
