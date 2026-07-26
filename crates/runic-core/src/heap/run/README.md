# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, `SEGMENT_SIZE`, freelist-primary ownership, RemotePending-only `BlockStates`, and segment header publish/recover (`publish_segment` / `from_block_ptr`).
- `heap.rs`: `RunHeap` with `Arena<Run>`, available-run lists, segment-aligned mmap, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one segment-aligned mapping and one size class. Layout: header page + `RUN_SIZE` payload + one `AtomicU8` per block for remote-pending bits; `Run::range` is the payload span only.
- Any block pointer recovers the arena `Run` via `Run::from_block_ptr` (mask → header magic → pointer). Runs do not stamp PageMap.
- Returned blocks must be valid block boundaries inside the payload span.
- Owner Free/Live is freelist membership (+ bump). Freelist head and intrusive payload links use raw `usize` / `FREE_END`. Allocate pops/bumps then `live++` with no owner `BlockStates` store.
- Owner double-free poison is freelist-head identity (immediate double-free).
- `BlockStates` tracks RemotePending only (clear ↔ pending CAS). Never-issued indices are rejected via owner `bump` / cold `issued`.
- `Run::free` / `accept` return `Result<(), RunError>`; `RunHeap` reads `is_full()` before those ops for available-list `finish_free`. Sticky TLS free calls `Run::free` only.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Sticky TLS caches hold a run checked out from `available[]`; reincarnation rebinds every occupied arena run, including sticky ones.
- Live small ownership for reclaim is `RunHeap::has_live_blocks` over occupied arena runs (live + remote-pending).
- Runs stay arena-resident with live segment headers for the heap lifetime in v0.5 (no empty-run OS release).
