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
4. **Remote Active:** `claim_free` (run block → `RemotePending`, or extent pending) → coalesce onto the freer TLS `RemoteBatch` → publish any returned `RemoteList` via `HeapTable::publish` without blocking on the owner.
5. **Remote Draining (mode snapshot):** under table lock, flush inbox then exclusive free; may reclaim when empty.

The freeing thread must not complete into the owner freelist while the owner is `Active`.

## Remote inbox

Each `Heap` owns a lock-free MPSC `Inbox` (intrusive links in remote-pending blocks). Enqueue is lock-free for `Active` heaps and must never block on the owner (see remote-free burst / #61).

Transport is batched: `RemoteBatch` on `ThreadHeap` coalesces; capacity or target-change returns a `RemoteList` the freer must publish. A returned list is an obligation — publish, drain-complete, or abort; never drop claimed nodes.

Owner drains via `Heap::flush`:

- alloc miss (freelist empty): flush → retry same cached run → then new run/mmap
- slow local free / take_run paths when inbox non-empty
- thread exit and Draining flush under table lock

`flush` routes each pointer through `PageMap` (run or extent) and completes the free, decrementing `alloc_count`.

## Claim → publish contract

```text
1. Resolve HeapId → slot; load mode + generation
2. Fail if Free or generation mismatch
3. claim_free(ptr) → Pending
4. Append to TLS RemoteBatch for HeapId
5. If a RemoteList is returned (capacity or target change), `HeapTable::publish` it:
   - Active: Inbox::push_batch
   - Draining: push_batch then flush under table lock (may reclaim)
   - Free / stale gen: fail (do not drop the list)
6. Retained partial batches publish later the same way (TLS release, later free)
```

Late frees after owner exit remain valid: publish of a retained batch against a `Draining` heap completes under the table lock.

## Synchronization

- Remote producers: atomics only (`claim` + MPSC inbox push while Active).
- Active owner: exclusive `&mut Heap` via TLS (no run/extent mutex).
- Draining: table lock for exclusive flush/reclaim (no separate orphan lock); late batch publish uses the same lock path.
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
- retained TLS batch published after owner → Draining
- target-change publish while previous heap is Draining
- stale generation fails cleanly
- randomized cross-thread traces
