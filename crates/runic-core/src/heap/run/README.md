# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, and per-block `AtomicU8` state (`BlockStates`).
- `heap.rs`: `RunHeap` with `Arena<Run>`, available-run lists, page-map publication, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one mapping and one size class.
- Returned blocks must be valid block boundaries inside the run mapping.
- `BlockStates` is the only free / allocated / remote-pending tracker (one `AtomicU8` per block, capacity = smallest class worst case).
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Sticky TLS caches hold a run checked out from `available[]`; reincarnation rebinds every occupied arena run, including sticky ones.
- Runs stay published and arena-resident for the heap lifetime in v0.5 (no empty-run OS release).
