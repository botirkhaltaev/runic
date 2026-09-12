# heap/run

Run metadata owns small size-class allocations.

## Files

- `mod.rs`: `Run`, `RunId`, pointer freelist + `extend`, in-page header at `base + RUN_SIZE`, and a private claim bitmap after the header.
- `config.rs`: `RunConfig` / `RunPolicy::{Keep,Discard}`.
- `heap.rs`: `RunHeap` with `Arena<NonNull<Run>>` (in-space headers) then `Arena<Mapping>`, available-run lists, payload-only page-map publication, and `HeapId` rebind.

## Invariants

- A run owns one size class and one range in a heap-owned map (not its own `Mapping`). The base is `RUN_SIZE`-aligned. Payload is `RUN_SIZE` bytes; the `Run` header lives at `base + RUN_SIZE` (`base`/`span`/`recip` next to `RunState`; remote `issued`/`link`/`claims` on the next 64-byte line); claim words follow the header. `Run::range` is the payload span only. `PageMap::publish_run` stamps that payload. A map holds `MAP_RUNS` spaces (`RUN_SPACE` each). Small free uses `Run::header_of` (`ptr & !(RUN_SIZE-1)`); self-check `base`, else `PageMap`.
- Returned blocks must be valid block boundaries inside the payload span.
- `locate` is offset from the run base. `Run::header_of` is the small-free / realloc probe.
- Owner Free/Live **authority** is freelist membership + `live` (+ bump). `allocate` is pop only. Empty freelist → `extend` threads one page of fresh blocks (at least 32, or remaining) and advances `issued` once. Hit free is `release` (`locate` → `live--` → pointer push). `free` also reports `was_full` and may discard. Owner double-free is undefined.
- Freelist head and intrusive payload links are payload addresses (`0` = end).
- Remote admission is the private claim bitmap. `claim` is `issued` + `try_set` (second claim is `DoubleFree`). `accept` drains bits onto the freelist.
- `Run` embeds an `InboxLink` (see `heap::inbox`) coalescing remote frees by run. Repeat claims while already queued do not re-link.
- `Run::accept` (owner-only, via `Heap::flush`) clears queued before scanning claim words so a racing claim is never dropped — the racer requeues, or accept asks the owner to push again.
- Interior / foreign pointers fail closed via `locate` / `PageMap`. Never-issued remote claims are rejected via `issued`.
- `Run::free` returns `Result<bool, RunError>` (`Ok(true)` when the run was full; `OutOfRange` is current-run miss; `InvalidPointer` is interior). `accept` returns `bool` (needs re-push). `RunHeap` calls `push_available` from that flag.
- `RunHeap` available-list pointers must refer to live in-space headers. A run is on the list at most once (`RunState.on_available`); `push_available` is idempotent. The current run may be on the list. `unbind` returns non-full current runs to the list.
- Alloc miss checks out a run from `available[]` (or take/map), `extend`s if needed, and sets TLS `current`. Reincarnation rebinds every occupied in-space header.
- Live small ownership for reclaim is `Heap::run_live` (0↔1 edges on `allocate` / `release` / `accept`). `RunHeap::has_live` still scans for isolated tests.
- Runs stay published and arena-resident for the heap lifetime. `RunPolicy::Discard` drops empty-run payload pages via `madvise`; the heap map stays. `Keep` leaves pages resident.
