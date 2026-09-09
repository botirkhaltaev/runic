# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, pointer freelist + `extend`, and a private claim bitmap in the space tail.
- `config.rs`: `RunConfig` / `RunPolicy::{Keep,Discard}`.
- `cache.rs`: `RunCache` — one-entry TLS payload-range probe (`hit` / `store` / `clear`). Not heap retention.
- `heap.rs`: `RunHeap` with `Arena<Run>` then `Arena<Mapping>`, available-run lists, payload-only page-map publication, and arena-wide `HeapId` rebind.

## Invariants

- A run owns one size class and one range in a heap-owned map (not its own `Mapping`). The base is `RUN_SIZE`-aligned. Payload is `RUN_SIZE` bytes, pad to 8-byte alignment, then `AtomicU64` claim words; `Run::range` is the payload span only. `PageMap::publish_run` stamps that payload. A map holds `MAP_RUNS` spaces (`RUN_SPACE` each).
- Returned blocks must be valid block boundaries inside the payload span.
- `Run` packs `base` / `span` / `recip` next to `RunState` (`free` / `live` / `capacity` first). `locate` is offset from the run base. `RunCache` is a `RUN_SIZE` range probe.
- Owner Free/Live **authority** is freelist membership + `live` (+ bump). `allocate` is pop only. Empty freelist → `extend` threads one page of fresh blocks (at least 32, or remaining) and advances `issued` once. `free` is `locate` → `live--` → pointer push. Owner double-free is undefined.
- Freelist head and intrusive payload links are payload addresses (`0` = end).
- Remote admission is the private claim bitmap. `claim` is `issued` + `try_set` (second claim is `DoubleFree`). `accept` drains bits onto the freelist.
- `Run` embeds an `InboxLink` (see `heap::inbox`) coalescing remote frees by run. Repeat claims while already queued do not re-link.
- `Run::accept` (owner-only, via `Heap::flush`) clears queued before scanning claim words so a racing claim is never dropped — the racer requeues, or accept asks the owner to push again.
- Interior / foreign pointers fail closed via `locate` / `PageMap`. Never-issued remote claims are rejected via `issued`.
- `Run::free` returns `Result<bool, RunError>` (`Ok(true)` when the run was full; `OutOfRange` is current-run miss; `InvalidPointer` is interior). `accept` returns `bool` (needs re-push). `RunHeap` calls `push_available` from that flag.
- `RunHeap` available-list pointers must refer to live `Arena<Run>` entries. A run is on the list at most once (`RunState.on_available`); `push_available` is idempotent. The current run may be on the list. `unbind` returns non-full current runs to the list.
- Alloc miss checks out a run from `available[]` (or take/map), `extend`s if needed, and sets TLS `current`. Reincarnation rebinds every occupied arena run.
- Live small ownership for reclaim is `Run::is_live` (allocated or remote-claimed), aggregated by `RunHeap::has_live` over occupied arena runs.
- Runs stay published and arena-resident for the heap lifetime. `RunPolicy::Discard` drops empty-run payload pages via `madvise`; the heap map stays. `Keep` leaves pages resident.
