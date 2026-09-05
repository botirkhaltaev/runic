# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, freelist-primary ownership, clear/Free `BlockStates`, and a private claim bitmap in the mapping tail.
- `heap.rs`: `RunHeap` with `Arena<Run>`, available-run lists, page-map publication, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one mapping and one size class. The mapping is `RUN_SIZE` payload bytes, one `AtomicU8` Free bit per block, pad to 8-byte alignment, then `AtomicU64` claim words; `Run::range` is the payload span only.
- Returned blocks must be valid block boundaries inside the payload span.
- `Run` caches `stride` / `stride_shift` for `address`; `locate` checks the payload span, then `SizeClass::index_of`, then capacity (rejects tail slack).
- Owner Free/Live **authority** is freelist membership (+ bump). `allocate` is refill-only (pops or `bump`, then `live++`); `bump` stores no `BlockStates`; freelist allocate rejects in-flight claims then `set(Clear)`.
- Freelist head and intrusive payload links use raw `usize` / `FREE_END`.
- Remote admission is the private claim bitmap. Owner `free` (magazine `take` only) stores Free (Release) then rechecks the claim bit (Acquire): `Claimed` if the bit won (`accept` publishes), `DoubleFree` for owner DF. `claim` sets the bit then Acquire-loads Free. TLS hit does not call `free`.
- `Run` embeds an `InboxLink` (see `heap::inbox`) coalescing remote frees by run: `claim` sets a bit, then the freer `enqueue`s (Idle → Queued + link) and, only on a queue win, takes an Active enqueue lease. Repeat claims while Queued do not re-link.
- `Run::accept` (owner-only, via `Heap::flush`) is the paired drain: it clears queued *before* scanning every claim word, so a racing `claim` + `Inbox::push` on a block that lands in an already-scanned word is never dropped — either that racer's own push wins and requeues, or `accept` returns `true` and the owner pushes again. Exactly one of the two pushes (wakeup proof).
- `BlockStates` Free bit keeps delayed double-free fail-closed. Never-issued indices are rejected via owner `bump` / cold `issued`.
- `Run::free` returns `Result<_, RunError>` (`Claimed` / `DoubleFree` / `InvalidPointer`); `accept` returns `bool` (needs re-push). `RunHeap` reads `is_full()` before those ops for available-list relinking. Magazine `take` calls `Heap::free` → `Run::free` (no TLS hit).
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Refill checks out a run from `available[]` (or OS), fills the magazine, and returns it if not full. Reincarnation rebinds every occupied arena run.
- Live small ownership for reclaim is `Run::is_live` (allocated or remote-claimed), aggregated by `RunHeap::has_live` over occupied arena runs.
- Runs stay published and arena-resident for the heap lifetime in v0.5 (no empty-run OS release).
