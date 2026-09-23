# heap/run

Run metadata owns small size-class allocations. Hit/miss:
[ARCHITECTURE.md](../../../../../ARCHITECTURE.md).

## Files

- `mod.rs`: `Run`, `RunId`, pointer freelist + `extend`, in-page header at `base + RUN_SIZE`, and a private claim bitmap after the header.
- `config.rs`: `RunConfig` / `RunPolicy::{Keep,Discard}`.
- `heap.rs`: `RunHeap` with `Arena<&'static Run>` (in-space headers) then `Arena<Mapping>`, available-run lists, and payload-only page-map publication.

## Invariants

- A run owns one size class in a heap-owned map. Its base is
  `RUN_SIZE`-aligned; the payload is `RUN_SIZE` bytes.
- The header follows the payload. Claim words follow the header. `PageMap`
  publishes the payload only.
- Returned pointers must be block boundaries inside the payload.
  `Run::header_of` validates the raw base word before constructing `Run`.
- Freelist membership and `live` decide Free vs Live. `allocate` pops. Empty
  freelists call `extend`, which adds one page of fresh blocks, at least 32,
  and advances `issued`. Hit free is `locate` then push. Owner double-free is
  undefined.
- Freelist head and intrusive payload links are payload addresses (`0` = end).
- Remote admission is `issued` plus `try_set` on the claim bitmap. A second
  claim is `DoubleFree`. `accept` drains bits onto the freelist.
- The embedded inbox link coalesces remote frees by run. `accept` clears the
  queued flag before scanning so racing claims can requeue.
- Interior and foreign pointers fail closed. Never-issued remote claims fail
  via `issued`.
- Available-list membership is unique and `push_available` is idempotent. The
  current run may also be listed.
- Runs remain published for the heap lifetime. `Discard` releases empty payload
  pages with `madvise`; `Keep` leaves them resident.
