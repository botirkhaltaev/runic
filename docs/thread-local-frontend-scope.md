# Thread-Local Heap Implementation Scope

Issue: #25

Runic v0.5 should implement full thread-local heaps for small allocations. This
includes owner identity, local small allocation and free paths, remote-free
routing, and thread-exit cleanup in one correctness-focused milestone.

## Design Boundary

Use explicit shared and local entities:

```text
Allocator
  -> SharedHeap
      -> PageMap
      -> RunHeap
      -> ExtentHeap
      -> HeapRegistry
  -> LocalHeap
      -> HeapId
      -> local available run lists
```

Do not clone the current global heap per thread. `PageMap`, `RunArena`, and
`ExtentHeap` stay shared because they own global pointer routing, stable metadata
lifetime, and dedicated allocation policy.

## Entities

```text
SharedHeap
  owns PageMap, RunHeap, ExtentHeap, HeapRegistry

LocalHeap
  owns one HeapId and per-class local run lists

RunHeap
  owns RunArena, shared available lists, and run mapping cache

Run
  owns one mapping, one size class, block states, owner, and list links

HeapRegistry
  owns HeapId slots, owner run lists, and fixed remote-free inboxes
```

`Run` metadata remains in `RunArena`. A `LocalHeap` owns permission to allocate
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
    Shared,
    Thread(HeapId),
}
```

Do not add `Retired` as an owner state. Removed runs leave `RunArena`; live runs
owned by an exiting thread transfer back to `RunOwner::Shared`.

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
  -> LocalHeap::allocate(class)
  -> local run hit: allocate without shared lock
  -> local miss: lock SharedHeap and refill a thread-owned run
```

Large allocation:

```text
Allocator::alloc
  -> SharedHeap::allocate_extent
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
  -> LocalHeap::free_local(class, ptr)
  -> local hit: validate and free without shared lock
  -> local miss: lock SharedHeap and route through PageMap
```

Shared routed free:

```text
SharedHeap::free_routed
  -> PageMap lookup
  -> Extent: exact-pointer free through ExtentHeap
  -> RunOwner::Shared: shared run free
  -> RunOwner::Thread(current): local fallback or metadata error
  -> RunOwner::Thread(other): mark remote pending and enqueue RemoteFree
```

The page map remains the source of truth for unknown pointers.

## Remote Frees

v0.5 should use shared-lock remote routing first. Do not start with lock-free
queues.

Remote free steps:

```text
lock SharedHeap
PageMap lookup
validate target Run
validate target owner HeapId
mark block Allocated -> RemotePending
enqueue RemoteFree { run, ptr } into HeapRegistry inbox
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

`HeapRegistry` should track all runs assigned to each `HeapId`, not only runs in
local available lists. Full local runs are not available but still need ownership
transfer on thread exit.

Use explicit list names:

```text
available_next: per-class allocation list
owner_next: per-heap ownership list
```

Thread exit steps:

```text
LocalHeap::drop
  -> lock SharedHeap
  -> drain remote inbox for HeapId
  -> walk owner run list
  -> RunOwner::Thread(id) -> RunOwner::Shared
  -> move reusable runs to shared available lists
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

1. Rename current `Heap` to `SharedHeap`.
2. Add `HeapId`, `RunOwner`, and `HeapRegistry`.
3. Add `owner` and `owner_next` to `Run`.
4. Keep `PageOwner::Run(NonNull<Run>)` pointing at stable run metadata.
5. Replace `FreeBitmap` behavior with explicit block-state transitions.
6. Add `LocalHeap` with fixed per-class run lists.
7. Add guarded TLS access in `Allocator`.
8. Implement shared refill into `RunOwner::Thread(heap_id)`.
9. Implement local small allocation fast path.
10. Implement local small free fast path.
11. Implement remote-free marking and inbox enqueue.
12. Implement remote drain.
13. Implement thread-exit ownership transfer.
14. Add cross-thread tests and benchmark reporting.
15. Update README and roadmap for the v0.5 release.

## Tests

Required unit and integration coverage:

```text
HeapRegistry reserves and releases HeapId
HeapRegistry tracks all runs assigned to a heap
Run rejects invalid owner transitions
Run block states detect double free
Run block states detect double remote free
RemotePending drains to Reusable
LocalHeap allocates from local run
LocalHeap frees local pointer
LocalHeap rejects interior pointer
SharedHeap routes remote free to target inbox
thread retirement transfers all owned runs to Shared
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
