# Thread-Local Heap Implementation Scope

Issue: #25

Runic v0.5 should implement full thread-local heaps for small allocations. This
includes owner identity, local small allocation and free paths, remote-free
routing, and thread-exit cleanup in one correctness-focused milestone.

## Design Boundary

Use explicit core, heap table, and thread-heap entities:

```text
Allocator
  -> AllocatorCore
      -> PageMap
      -> Heap
          -> RunHeap
          -> ExtentHeap
      -> HeapTable
          -> ThreadHeap

Heap
  -> RunHeap
  -> ExtentHeap

ThreadHeap
  -> HeapId
  -> HeapSlot pointer
  -> small-run frontend only
```

Do not clone the full allocator backend per thread. `PageMap` stays core-owned,
extents remain central, and thread heaps only front small-run allocation.

## Entities

```text
AllocatorCore
  owns PageMap and locked AllocatorState

HeapTable
  owns HeapId slots, HeapSlot storage, and fixed remote-free inboxes

ThreadHeap
  owns one retained AllocatorCore reference and current HeapId for one thread

RunHeap
  owns RunArena and per-size-class available run lists

Run
  owns one mapping, one size class, block states, owner, and list links

ExtentHeap
  owns central dedicated allocation policy and mapping reuse
```

`Run` metadata remains in `RunArena`. A `ThreadHeap` owns permission to allocate
from runs assigned to its `HeapId`, not the physical metadata object. This keeps
`PageMap` pointers stable after thread exit.

## Core Types

Use tuple newtypes for opaque scalar IDs:

```rust
pub(crate) struct HeapId(NonZeroU32);
```

Use named fields for entities that own multiple invariants:

```rust
pub(crate) struct Run {
    id: RunId,
    owner: RunOwner,
    mapping: Mapping,
    range: AddressRange,
    class: SizeClassId,
    block_size: usize,
    block_shift: Option<u32>,
    capacity: u32,
    live: u32,
    available_next: Option<NonNull<Run>>,
    owner_next: Option<NonNull<Run>>,
    blocks: BlockStates,
}
```

```rust
pub(crate) enum RunOwner {
    Central,
    Thread(HeapId),
}
```

Do not add `Retired` as an owner state. Removed runs leave `RunArena`; live runs
owned by an exiting thread remain in stable metadata until freed or reclaimed.

## Block States

The current free/allocated bitmap is not enough for remote frees. v0.5 needs
explicit block-state transitions:

```text
Reusable
Allocated
RemotePending
```

Valid transitions:

```text
Reusable -> Allocated          allocation
Allocated -> Reusable          local or shared free
Allocated -> RemotePending     remote free accepted
RemotePending -> Reusable      owner drains remote free
```

Invalid transitions must report domain errors that abort at the allocator
boundary:

```text
Reusable -> free
RemotePending -> free
RemotePending -> remote pending
```

The representation can remain bitmap-backed, but the API should expose domain
transitions directly. Reshape `Run::allocate` and `Run::free` instead of adding
parallel compatibility methods.

## Allocation Path

Small allocation:

```text
Allocator::alloc
  -> classify layout with SizeClasses
  -> ThreadHeap current run hit: allocate without AllocatorState lock
  -> local miss: lock AllocatorState and allocate/refill a thread-owned run
```

Large allocation:

```text
Allocator::alloc
  -> central Heap::allocate_extent
  -> ExtentHeap
```

Local heaps must not own extents. Extents remain shared because dedicated
allocation policy, exact-pointer validation, and mapping retention belong to
`ExtentHeap` and `ExtentCache`.

## Free Path

Small free:

```text
Allocator::dealloc
  -> classify layout with SizeClasses
  -> current ThreadHeap local hit: validate and free without AllocatorState lock
  -> local miss: lock AllocatorState and route through PageMap
```

Shared routed free:

```text
AllocatorState::dealloc
  -> PageMap lookup
  -> Extent: exact-pointer free through ExtentHeap
  -> RunOwner::Central: central run free
  -> RunOwner::Thread(current): local fallback or metadata error
  -> RunOwner::Thread(other): mark remote pending and enqueue RemoteFree
```

The page map remains the source of truth for unknown pointers.

## Remote Frees

v0.5 should use shared-lock remote routing first. Do not start with lock-free
queues.

Remote free steps:

```text
lock AllocatorState
PageMap lookup
validate target Run
validate target owner HeapId
mark block Allocated -> RemotePending
enqueue RemoteFree { run, ptr } into HeapTable inbox
```

The inbox must be fixed-capacity and allocation-free. If enqueue fails, drain the
target inbox and retry once. If it still fails, return a metadata error that
aborts at the allocator boundary. Never drop a free.

Remote drains happen on owner slow paths and thread exit:

```text
pop RemoteFree
authenticate RunId and pointer against stable Run metadata
RemotePending -> Reusable
```

## Thread Exit

Thread exit is required for v0.5.

Thread exit steps:

```text
ThreadHeap::drop
  -> lock AllocatorState
  -> drain remote inbox for HeapId
  -> release empty thread-owned runs
  -> release HeapId
```

Live allocations remain valid because run metadata stays in `RunArena` and
`PageMap` still points to stable `Run` records.

## TLS Rules

Allocator TLS initialization can recurse. Use an explicit TLS state guard:

```text
Uninitialized
Initializing
Ready
Dropping
```

If the current TLS state is `Initializing` or `Dropping`, use the shared path.
The fallback must preserve correctness, even if it misses the local fast path.

## Implementation Order

1. Keep `AllocatorCore` as owner of `PageMap` and locked `AllocatorState`.
2. Keep `Heap` as the owner of `RunHeap` and central `ExtentHeap` policy.
3. Add `HeapId`, `HeapTable`, `HeapSlot`, and thread-local `ThreadHeap` handles.
4. Keep `PageOwner::Run(NonNull<Run>)` pointing at stable run metadata.
5. Represent run ownership as `RunOwner::Central | RunOwner::Thread(HeapId)`.
6. Represent reusable, allocated, and remote-pending block states explicitly.
7. Implement same-thread small allocation and free fast paths.
8. Route large allocations through central extents.
9. Implement remote-free marking and inbox enqueue.
10. Implement owner-side remote drain.
11. Release empty thread-owned runs on thread exit/reclaim.
12. Add cross-thread tests and benchmark reporting.
13. Update README and roadmap for the v0.5 release.

## Tests

Required unit and integration coverage:

```text
HeapTable reserves and releases HeapId generations
ThreadHeap retains and releases AllocatorCore ownership
Run reports explicit owner identity
Run block states detect double free
Run block states detect double remote free
RemotePending drains to Reusable
ThreadHeap allocates from thread-owned run
ThreadHeap frees local pointer
ThreadHeap rejects interior pointer
AllocatorState routes remote free to target inbox
thread retirement drains remote frees and releases empty runs
same-thread local double free aborts
same-thread local interior free aborts
cross-thread free succeeds
double cross-thread free aborts
free after allocating thread exits succeeds
realloc after remote free aborts
large extent behavior remains shared
randomized cross-thread traces pass
```

## Benchmarks

The implementation must report:

```text
threaded/thread_local_churn/runic/2
threaded/thread_local_churn/runic/4
threaded/mixed_thread_random/runic/2
threaded/mixed_thread_random/runic/4
threaded/cross_thread_free_ring/runic/2
threaded/cross_thread_free_ring/runic/4
explicit/single_size_churn/runic/64
explicit/small_biased_random/runic
```

The feature is not ready if it improves threaded churn by weakening invalid-free,
double-free, stale-free, remote-free, or thread-exit behavior.
