# Thread-Local Heap Plan

Issue: #8

Runic v0.5 uses TLS owner-local heaps for small runs and large extents. Slow paths
still coordinate through `AllocatorState` / `HeapTable` for acquire and release;
steady-state hits avoid that lock.

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

Target workloads:

- `threaded/thread_local_churn`
- `threaded/mixed_thread_random`
- randomized cross-thread traces

## Lessons From #16

A global-lock small-object cache is not enough by itself. Thread-local heaps help
only when a local hit avoids meaningful shared metadata work.

## v0.5 Shape (implemented)

- Every `Run` / `Extent` stores `HeapId` (no root/central ownership heap).
- TLS `ThreadHeap` caches runs; hot alloc/free need no table lock.
- Alloc miss: flush remote inbox → retry → then take/create run.
- Remote free: claim → lock-free `Inbox` push; owner/Draining flush completes.
- Slot lifecycle: `Free | Active | Draining` with generation bump on reclaim.
- Extents are heap-local with the same remote protocol.

See [thread-local-frontend-scope.md](thread-local-frontend-scope.md) and
[remote-free-protocol.md](remote-free-protocol.md).

## Acceptance For #25

- `threaded/thread_local_churn` improves materially over the global-lock baseline.
- Abort tests for invalid free, double free, and realloc-after-free still pass.
- Remote frees are validated and never silently dropped.
- Thread exit with live allocations remains valid; late frees complete under Draining.
- No allocator-internal heap allocation is introduced.
