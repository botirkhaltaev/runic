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
- Remote admission is exclusively `ClaimBits`. Owner `free` stores Free (Release) then rechecks the claim bit (Acquire); `claim` sets the bit then Acquire-loads Free.
- `Run` embeds a `Notify` (see `heap::table::inbox`) coalescing remote frees by run: `claim` sets a bit, then the freer calls `try_arm` (Idle → Queued) and, only on a win, publishes the run to `HeapDirectory`. Repeat claims while Queued do not republish.
- `Run::accept_remote` (owner-only, via `HeapSlot::flush`) is the paired drain: it disarms (Queued → Idle) *before* scanning every `ClaimBits` word, so a racing `claim` + `try_arm` on a block that lands in an already-scanned word is never dropped — either that racer's own `try_arm` wins and republishes, or it loses to this call's own re-arm check (`any_set` + `try_arm`) after the scan. Exactly one of the two republishes (wakeup proof).
- `BlockStates` Free bit keeps delayed double-free fail-closed. Never-issued indices are rejected via owner `bump` / cold `issued`.
- `Run::free` / `accept_remote` return `Result<_, RunError>`; `RunHeap` reads `is_full()` before those ops for available-list relinking. Sticky TLS free calls `Run::free` only.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Sticky TLS caches hold a run checked out from `available[]`; reincarnation rebinds every occupied arena run, including sticky ones.
- Live small ownership for reclaim is `RunHeap::has_live_blocks` over occupied arena runs (live + remote-claimed).
- Runs stay published and arena-resident for the heap lifetime in v0.5 (no empty-run OS release).
