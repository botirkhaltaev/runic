# heap/run

Run metadata owns small size-class allocations. Hit/miss:
[ARCHITECTURE.md](../../../../../ARCHITECTURE.md).

## Files

- `mod.rs`: `Run`, `RunId`, `extend`, in-page header at `base + RUN_SIZE`. The `safe` build stores a private claim bitmap after the header.
- `freelist.rs`: owner-exclusive free-block stack. The head and each free block's first word are payload addresses (`0` = end).
- `config.rs`: `RunConfig` / `RunPolicy::{Keep,Discard}`.
- `heap.rs`: `RunHeap` with `Arena<&'static Run>` (in-space headers) then `Arena<Mapping>`, available-run lists, `Hints` on payload maps, and payload-only page-map publication.

## Invariants

- A run owns one size class in a heap-owned map. Its base is
  `RUN_SIZE`-aligned; the payload is `RUN_SIZE` bytes.
- The header follows the payload. On `safe`, claim words follow the header.
  `PageMap` publishes the payload only.
- Returned pointers must be block boundaries inside the payload.
  `Run::header_of` validates the raw base word before constructing `Run`.
- Freelist membership and `live` decide Free vs Live. `allocate` pops. Empty
  freelists call `extend`, which adds one page of fresh blocks, at least 32,
  and advances `issued`. Hit free is `locate` then push. Owner double-free is
  undefined on Fast. `--features safe` aborts it: `Freelist::ensure_absent`
  walks the stack only when the block's first word is `0` or a block address.
- Freelist head and intrusive payload links are payload addresses (`0` = end).
- Remote admission is `claim`. An index past `issued` is `DoubleFree`. On
  `safe`, a second claim of an issued block is also `DoubleFree`, including
  while the block sits on a thread slot. Fast leaves that second claim
  undefined. The freer `hold`s claimed blocks on a slot and `Run::push`
  prepends that chain. `accept` stores the inbox link idle and splices the chain onto the
  freelist. `safe` clears those bits. `Requeue` means a `push` landed after
  the take.
- The inbox link is one atomic. Idle is a sentinel, never null. `queue` CASes
  idle to pushing, then links. A non-idle link coalesces by run.
- Interior and foreign pointers fail closed. Never-issued remote claims fail
  via `issued`.
- Available-list membership is unique and `push_available` is idempotent. The
  current run may also be listed.
- Runs remain published for the heap lifetime. `Discard` releases empty payload
  pages with `madvise`; `Keep` leaves them resident. There is no run cache
  budget because maps remain heap-owned; whole-map budget/reclaim belongs to
  the 0.12 reclaim milestone.
