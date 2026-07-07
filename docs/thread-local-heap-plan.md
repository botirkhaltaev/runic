# Thread-Local Heap Plan

Issue: #8

Runic v0.4 still uses one global heap lock. That is correct for the current correctness, single-thread optimization, and retention-policy milestone, but threaded benchmarks show the expected ceiling for this architecture.

## Current Signal

Recent local comparison runs show Runic is most constrained by global serialization on threaded small-allocation workloads:

- `compare/threaded/thread_local_churn/runic/4`: about `200 us`.
- `compare/threaded/thread_local_churn/system/4`: about `61 us`.
- `compare/threaded/thread_local_churn/mimalloc/4`: about `65 us`.
- `compare/threaded/thread_local_churn/snmalloc/4`: about `61 us`.
- `compare/threaded/mixed_thread_random/runic/4`: about `377 us`.
- `compare/threaded/mixed_thread_random/system/4`: about `113 us`.
- `compare/threaded/mixed_thread_random/mimalloc/4`: about `138 us`.
- `compare/threaded/mixed_thread_random/snmalloc/4`: about `147 us`.

The v0.5 thread-local design should target these workloads:

- `threaded/thread_local_churn`
- `threaded/mixed_thread_random`
- later, randomized cross-thread traces once remote-free ownership exists

## Lessons From #16

A global-lock small-object cache is not enough by itself. The rejected #16 cache attempts either weakened stale-free detection or added enough state traffic to regress single-thread workloads.

Thread-local heaps should therefore be introduced only when a local hit can avoid meaningful global metadata work, not merely move blocks between two global-lock-owned containers.

## v0.5 Shape

The local heap should be narrow but complete:

- Small allocations only.
- One local cache per size class.
- Refill from shared `RunHeap` under the global lock in fixed batches.
- Return or drain blocks to shared run metadata through explicit operations.
- Route remote frees through the shared heap with owner-side validation.

The shared allocator remains responsible for:

- mmap-backed run creation
- page-map publication
- run table ownership
- extent allocation
- invalid free policy

The local heap owns only cached small blocks that are safe to allocate without touching shared metadata on every hit.

## Required Ownership Rules

Before implementation, Runic needs an explicit representation for cached block ownership:

- A block in a local cache must not be reported as a live user allocation.
- A stale free of a locally cached block must not be accepted as a valid free.
- A cached block must not also be reachable from the run bitmap as available.
- A local cache must know which run or future region/span owns each cached block.
- The design must define what happens when the freeing thread is not the owning local heap.

These requirements point directly at #24 and #27. A span/region ownership model may make cached block ownership easier to encode, and the remote-free protocol must exist before local heaps handle cross-thread frees efficiently.

## Implementation Boundary

The v0.5 implementation should not attempt unrelated allocator features.

In scope:

- Thread-local small allocation hits.
- Batch refill from global run metadata.
- Explicit local cache drain and ownership transfer on thread exit.
- Shared-lock remote-free routing with fixed inboxes.
- Existing global path fallback for large allocations and uncommon cases.

Out of scope:

- NUMA placement.
- Hugepage policy.
- Per-CPU caches.
- Lock-free remote-free queues before the shared-lock protocol is proven.
- Hardening features before #28.

## Acceptance For #25

The implementation PR for #25 should show:

- `threaded/thread_local_churn` improves materially over the global-lock baseline.
- `threaded/mixed_thread_random` improves or the remaining blocker is identified.
- Abort tests for invalid free, double free, and realloc-after-free still pass.
- Cached block ownership is explicit in types or state transitions, not implicit in comments.
- Remote frees are validated and never silently dropped.
- Thread exit with live allocations remains valid.
- No allocator-internal heap allocation is introduced.
