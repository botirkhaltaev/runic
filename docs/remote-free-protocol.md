# Remote-Free Ownership Protocol

Issue: #27

Remote frees must not mutate another heap's freelist or available-run lists directly. Ownership is `HeapId` on every `Run` and `Extent`.

## Ownership Identity

```text
HeapId          slot index + generation
Run.heap_id     HeapId
Extent.heap_id  HeapId
PageMap lookup  Run pointer | Extent pointer
```

There is no central/root ownership heap. Metadata stays immovable in arenas so `PageMap` pointers remain valid after thread exit.

## Free Routing

On free:

1. Look up the pointer in `PageMap`.
2. Compare TLS `HeapId` to the entity `HeapId`.
3. **Local:** exclusive `Heap` free (cached run: `free_local`; else flush inbox if needed then free).
4. **Remote Active:** `claim_free` (run block → `RemotePending`, or extent pending) → re-check slot gen/mode → `Inbox::push` without holding the table lock.
5. **Remote Draining:** under table lock, flush inbox then exclusive free; may reclaim when empty.

The freeing thread must not complete into the owner freelist while the owner is `Active`.

## Remote inbox

Each `Heap` owns a lock-free MPSC `Inbox` (intrusive links in remote-pending blocks). Enqueue is lock-free and must never block on the owner (see remote-free burst / #61).

Owner drains via `Heap::flush`:

- alloc miss (freelist empty): flush → retry same cached run → then new run/mmap
- slow local free / take_run paths when inbox non-empty
- thread exit and Draining flush under table lock

`flush` routes each pointer through `PageMap` (run or extent) and completes the free, decrementing `alloc_count`.

## Claim → push contract

```text
1. Resolve HeapId → slot; load mode + generation
2. Fail if Free or generation mismatch
3. claim_free(ptr) → Pending
4. Re-check mode ∈ {Active, Draining} and generation
5. On failure: unclaim/abort (no orphan Pending)
6. inbox.push(ptr)
7. If Draining: flush under table lock then free directly
```

## Synchronization

- Remote producers: atomics only (`claim` + MPSC inbox push).
- Active owner: exclusive `&mut Heap` via TLS (no run/extent mutex).
- Draining: table lock for exclusive flush/reclaim (no separate orphan lock).
- `alloc_count`: Pending counts as live; reclaim only when count is 0 and inbox empty.

## Failure Behavior

Invalid pointers and double frees abort (or report domain errors that abort at the allocator boundary). A free must never be silently dropped.

## Tests

- local free validates block boundaries / exact extent pointers
- remote free does not mutate owner freelists directly
- owner flush validates and returns blocks
- double remote free aborts / reports double free
- remote burst completes without owner progress (#61)
- exit + late remote free + draining reclaim
- stale generation fails cleanly
- randomized cross-thread traces
