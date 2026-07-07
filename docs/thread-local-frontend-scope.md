# Thread-Local Frontend Scope

Issue: #25

This document defines the first thread-local heap frontend implementation boundary. It intentionally does not implement the frontend yet because the required ownership protocol is still in stacked design PRs.

## Why Not Implement Immediately

A thread-local heap changes allocator ownership. Implementing it before owner identity and remote-free routing are represented in code would either:

- route remote frees through the global lock permanently
- let one thread mutate another thread's local metadata
- weaken invalid-free and double-free checks
- add a cache that does not improve the target workloads

The rejected #16 cache attempts show that moving blocks between global structures is not enough. The local path must avoid global metadata work on hits while still preserving validated frees.

## First Entities

The first implementation should introduce explicit entities rather than broad manager methods:

```text
LocalHeap
  id: HeapId
  small caches by SizeClassId
  remote-free inbox

SharedHeap
  current Heap responsibilities that remain global
  RunHeap
  ExtentHeap
  PageMap

HeapId
  non-zero owner identity
```

`Allocator` should route through thread-local `LocalHeap` for small allocations and fall back to `SharedHeap` for extents and uncommon paths.

## First Small Allocation Path

On small allocation:

1. Classify the layout with `SizeClasses`.
2. Try the local cache for that class.
3. If hit, mark the cached block user-allocated and return it.
4. If miss, refill a fixed batch from `SharedHeap` under the global lock.
5. Return one block and keep the rest local.

The local cache must not allocate internally.

## First Free Path

On free:

1. Look up the pointer in `PageMap`.
2. Determine run owner identity.
3. If local, validate block state and return to local cache or owner run state.
4. If remote, enqueue a `RemoteFree` message as defined by #27.
5. If queue enqueue fails in the first implementation, use a validated global-lock fallback.

The page map remains the source of truth for unknown pointer detection.

## Required Block States

The local cache requires block state beyond a single allocated/free bit:

- free in run block bitmap
- owned by local cache
- user allocated
- optionally pending remote free

This must be encoded in run-owned state or a future region/span-owned state. It must not be inferred from comments or cache membership alone.

## Thread Exit

The first implementation must define thread-exit behavior before enabling local caches:

- drain local caches to the shared owner metadata
- drain outbound remote frees if possible
- make any remaining remote-free inbox reachable by shared cleanup

If Rust TLS destructor behavior is used, the allocator must not allocate while draining.

## Minimal Milestone

The first code PR for this feature should implement only:

- `HeapId`
- owner identity on runs or regions
- one thread-local `LocalHeap`
- small allocation cache hits and batch refill
- local free for local-owned blocks
- remote-free enqueue or validated fallback

Do not include:

- NUMA policy
- hugepages
- hardening policy
- adaptive cache sizing
- lock-free queues beyond the minimal bounded queue needed for remote frees

## Benchmarks

The implementation must report:

- `threaded/thread_local_churn/runic/2`
- `threaded/thread_local_churn/runic/4`
- `threaded/mixed_thread_random/runic/2`
- `threaded/mixed_thread_random/runic/4`
- abort-case tests for invalid free and double free

The feature is not ready if it improves thread-local churn by weakening remote-free behavior or invalid-free checks.

## Acceptance Gate

Before implementation, merge or directly incorporate the decisions from:

- #24 span ownership evaluation
- #27 remote-free protocol
- #16 cache constraints

This keeps the first thread-local heap implementation narrow, measurable, and compatible with Runic's correctness-first allocator invariants.
