# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, `RunOwner`, `RunBlock`, and bitmap-backed block state.
- `arena.rs`: out-of-line run metadata storage and reservations.
- `heap.rs`: small-allocation policy, run creation, available-run lists, and page-map publication.

## Invariants

- A run owns one mapping and one size class.
- Returned blocks must be valid block boundaries inside the run mapping.
- `BlockStates` is the source of truth for reusable, allocated, and remote-pending blocks.
- `RunHeap` available-list pointers must refer to live `RunArena` entries.
- Empty runs may be released only after page-map ownership and arena metadata are removed.
