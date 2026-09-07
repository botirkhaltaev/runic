# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, pointer freelist + `extend`, and a private claim bitmap in the mapping tail.
- `heap.rs`: `RunHeap` with `Arena<Run>`, available-run lists, page-map publication, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one mapping and one size class. The mapping is `RUN_SIZE` payload bytes, pad to 8-byte alignment, then `AtomicU64` claim words; `Run::range` is the payload span only.
- Returned blocks must be valid block boundaries inside the payload span.
- `Run` packs `span` / `recip` / `stride` next to `RunState`. `locate` is `offset >= span` then Lemire `stride | offset` via `(offset * recip) as u32 < recip` (rejects interior and tail slack). Index is `product >> 32` when callers need it. No jump table.
- Owner Free/Live **authority** is freelist membership + `live` (+ bump). `allocate` is pop only. Empty freelist → `extend` threads one page of fresh blocks (at least 32, or remaining) and advances `issued` once. `free` is `locate` → `live--` → pointer push. Owner double-free is undefined.
- Freelist head and intrusive payload links are payload addresses (`0` = end).
- Remote admission is the private claim bitmap. `claim` is `issued` + `try_set` (second claim is `DoubleFree`). `accept` drains bits onto the freelist.
- `Run` embeds an `InboxLink` (see `heap::inbox`) coalescing remote frees by run: `claim` sets a bit, then the freer `enqueue`s (Idle → Queued + link) and, only on a queue win, takes an Active enqueue lease. Repeat claims while Queued do not re-link.
- `Run::accept` (owner-only, via `Heap::flush`) is the paired drain: it clears queued *before* scanning every claim word, so a racing `claim` + `Inbox::push` on a block that lands in an already-scanned word is never dropped — either that racer's own push wins and requeues, or `accept` returns `true` and the owner pushes again. Exactly one of the two pushes (wakeup proof).
- Interior / foreign pointers fail closed via `locate` / `PageMap`. Never-issued remote claims are rejected via `issued`.
- `Run::free` returns `Result<bool, RunError>` (`Ok(true)` when the run was full; `InvalidPointer` only on the owner path). `accept` returns `bool` (needs re-push). `RunHeap` calls `push_available` from that flag.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries.
- Alloc miss checks out a run from `available[]` (or OS), `extend`s if needed, and sets TLS `current`. Reincarnation rebinds every occupied arena run.
- Live small ownership for reclaim is `Run::is_live` (allocated or remote-claimed), aggregated by `RunHeap::has_live` over occupied arena runs.
- Runs stay published and arena-resident for the heap lifetime in v0.5 (no empty-run OS release).
