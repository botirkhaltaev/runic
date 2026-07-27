# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, freelist-primary ownership, clear/Free `BlockStates`, and run-owned `ClaimBits`.
- `heap.rs`: `RunHeap` with `Arena<Run>`, available-run lists, page-map publication, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one mapping and one size class. The mapping is `RUN_SIZE` payload bytes, one `AtomicU8` Free bit per block, pad to 8-byte alignment, then `AtomicU64` claim words; `Run::range` is the payload span only.
- Returned blocks must be valid block boundaries inside the payload span.
- `Run` caches `stride` / `stride_shift` for `address`; `locate` checks the payload span, then `SizeClass::index_of`, then capacity (rejects tail slack).
- Owner Free/Live **authority** is freelist membership (+ bump). Allocate pops/bumps then `live++`; bump allocate stores no `BlockStates`; freelist allocate rejects in-flight claims then `set(Clear)`.
- Freelist head and intrusive payload links use raw `usize` / `FREE_END`.
- Remote admission is exclusively `ClaimBits`. Owner `free` stores Free (Release) then rechecks the claim bit (Acquire); `claim` sets the bit then Acquire-loads Free; `accept` `test_and_clear`s then freelist-publishes. Exactly one of free/accept pushes.
- `BlockStates` Free bit keeps delayed double-free fail-closed. Never-issued indices are rejected via owner `bump` / cold `issued`.
- `Run::free` / `accept` return `Result<(), RunError>`; `RunHeap` reads `is_full()` before those ops for available-list `finish_free`. Sticky TLS free calls `Run::free` only.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Sticky TLS caches hold a run checked out from `available[]`; reincarnation rebinds every occupied arena run, including sticky ones.
- Live small ownership for reclaim is `RunHeap::has_live_blocks` over occupied arena runs (live + remote-claimed).
- Runs stay published and arena-resident for the heap lifetime in v0.5 (no empty-run OS release).
