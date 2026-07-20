# Thread-Local Heap Implementation Scope

Issue: #25

Runic v0.5 implements owner-local heaps for small runs and large extents: `HeapId`
on entities, TLS hot paths, claim→inbox→flush remote frees, and Draining after
thread exit.

## Design Boundary

```text
Allocator
  -> AllocatorCore
      -> PageMap
      -> AllocatorState
          -> HeapTable { generations[], Arena<Heap> }
              -> ThreadHeap (TLS)

Heap
  -> mode (Free | Active | Draining)
  -> RunHeap / ExtentHeap
  -> alloc_count
  -> Inbox

Run / Extent
  -> HeapId
```

`PageMap` and `OsMemory` stay core-shared. Allocation ownership is never a process-wide
root heap: every run and extent is stamped with the allocating thread's `HeapId`.
Table/arena full returns null.

## Entities

```text
HeapTable
  owns Arena<Heap>, generations[], acquire/release, push_remote_batch

Heap
  owns mode, RunHeap, ExtentHeap, alloc_count, Inbox mailbox

ThreadHeap
  owns retained AllocatorCore ref, HeapId, Heap pointer, cached runs

Run
  owns mapping, size class, block states, HeapId

Extent
  owns dedicated mapping, HeapId, remote-pending flag
```

## Core Types

```rust
pub(crate) struct HeapId {
    slot: NonZeroU32,
    generation: NonZeroU32,
}

pub(crate) struct Heap {
    mode: AtomicU8, // Free | Active | Draining
    id: HeapId,
    runs: RunHeap,
    extents: ExtentHeap,
    alloc_count: Cell<u32>,
    inbox: Inbox,
}
```

## Block States (runs)

```text
Reusable -> Allocated          allocation
Allocated -> Reusable          local free
Allocated -> RemotePending     remote claim
RemotePending -> Reusable      owner flush
RemotePending -> Allocated     unclaim on failed enqueue (abort path)
```

## Allocation Path

Small:

```text
TLS cached run hit → allocate_local (no table lock, no flush)
freelist empty → Heap::flush → retry cached run → take_or_allocate_run
no heap → table get_or_acquire (table lock); fail → null
```

Large:

```text
TLS heap ExtentHeap allocate (Uninit / Zeroed); stamp HeapId; PageMap publish
```

## Free Path

```text
PageMap lookup
TLS HeapId == entity HeapId
  cached run → free_local
  else → flush if needed + exclusive free
else
  claim → HeapTable::push_remote_batch
  if Draining → flush under table lock / try_reclaim
```

## Thread Exit

```text
return cached runs
flush inbox
mode = Draining
re-flush until empty
try_reclaim if alloc_count == 0 (Free + generation bump)
else leave Draining for remote freers
```

Live allocations remain valid: metadata is immovable and `PageMap` still points at it.

## Perf Rules

1. Owner-local hit: zero mutexes, no inbox work.
2. Remote free: never wait on owner.
3. Alloc miss: flush before mmap (producer–consumer).

## Tests

```text
HeapTable acquire / generation bump on reclaim
ThreadHeap retains and releases AllocatorCore
Run/Extent report HeapId
block states detect double free and double remote free
alloc miss reuses remote frees before new mapping
remote burst without owner progress
thread exit + late remote free
draining reclaim
same-heap non-cached free does not self-enqueue
heap-local extents
randomized cross-thread traces
```
