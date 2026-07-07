# Remote-Free Ownership Protocol

Issue: #27

Remote frees are part of the v0.5 thread-local heap milestone. A free on a non-owner thread must not mutate another heap's local metadata directly.

## Ownership Identity

Every small-allocation source needs an owner identity.

Initial shape:

```text
HeapId
Run owner: Shared | Thread(HeapId)
PageMap lookup: Run pointer | Extent pointer
```

Run metadata remains stable in `RunArena`; owner identity lives on `Run` for v0.5. If a future `Region` entity owns publication and lifecycle, owner identity may move there directly.

## Free Routing

On free:

1. Look up the pointer in `PageMap`.
2. Validate that the pointer maps to a run or extent owner.
3. For extents, keep exact-pointer validation on shared metadata.
4. For runs, compare the current heap id to the run owner id.
5. If local, validate and free through local run/cache metadata.
6. If remote, enqueue a remote-free message for the owner heap.

The freeing thread must not directly touch the owner heap's local cache or reusable block state.

## Remote-Free Message

The minimum message is:

```text
RemoteFree
  run id
  pointer
```

The receiving heap already owns the run metadata and can validate:

- pointer belongs to the run
- pointer is a block boundary
- block is currently allocated
- block is not already cached or free

Do not trust remote-free messages as prevalidated frees.

## Queue Policy

The first queue should be allocation-free and bounded.

Recommended first version:

- one bounded inbox per owning heap in `HeapRegistry`
- fixed capacity chosen at allocator construction
- remote enqueue runs under the shared heap lock in v0.5
- owner drains on allocation slow path, deallocation slow path, and thread exit

If enqueue fails, drain the target inbox and retry once. If enqueue still fails, return an invalid-metadata error that aborts at the allocator boundary. A free must never be silently dropped.

## Synchronization Rules

Keep memory ordering minimal and explicit:

- Producer writes the message before publishing the queue slot.
- Consumer acquires the published slot before reading the message.
- Run block-state mutation happens only on the owner side.
- Queue slot reuse happens only after the consumer marks the slot empty.

No allocator-internal dynamic allocation is allowed for queue growth.

## Failure Behavior

Invalid pointers still abort at the allocator boundary.

Remote-free queue failure must not silently drop frees. The options are:

- validated global-lock fallback
- abort with a distinct invalid-metadata path
- drain owner queues before retrying, if the current thread owns the target heap

For v0.5, use shared-lock remote routing because it preserves correctness while keeping the remote-free queue bounded and auditable. Lock-free enqueue is a later optimization.

## Interaction With Thread-Local Heaps

The first thread-local heap implementation should use this protocol narrowly:

- local small allocations can hit local caches
- local frees return to local metadata
- remote frees enqueue to the owner
- large allocations remain shared extents
- page-map ownership remains the source of truth for pointer routing

## Tests Required For Implementation

An implementation PR should include:

- local free still validates block boundaries
- remote free does not mutate owner-local metadata directly
- owner drain validates and returns blocks
- double remote free aborts or reports double free at the allocator boundary
- queue-full fallback does not lose the free
- randomized cross-thread allocation/free traces

## Open Decisions

- Exact queue capacity and whether it is per heap or per size class.
- Whether owner identity should live directly on `Run` first or wait for a `Region` entity.
- How thread exit drains local caches and remote queues.
- Whether a remote free should ever publish directly to a shared central list.

These decisions should be resolved inside #25 before the v0.5 thread-local fast path is considered complete.
