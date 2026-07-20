# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, and bitmap-backed block state.
- `heap.rs`: small-allocation policy, `Arena<Run>`, available-run lists, and page-map publication.

## Invariants

- A run owns one mapping and one size class.
- Returned blocks must be valid block boundaries inside the run mapping.
- `BlockStates` is the source of truth for reusable, allocated, and remote-pending blocks.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Empty runs may be released only after page-map ownership and arena metadata are removed.
