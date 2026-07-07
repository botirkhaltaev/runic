# run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, `RunBlock`, and bitmap-backed block state.
- `arena.rs`: out-of-line run metadata storage and reservations.
- `cache.rs`: optional retained empty-run mapping cache.
- `heap.rs`: small-allocation policy, run creation, available-run lists, and page-map publication.

## Invariants

- A run owns one mapping and one size class.
- Returned blocks must be valid block boundaries inside the run mapping.
- `FreeBitmap` is the source of truth for reusable blocks and double-free detection.
- `RunHeap` available-list pointers must refer to live `RunArena` entries.
- Empty runs may be released only after page-map ownership and arena metadata are removed.
