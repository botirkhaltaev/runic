# Runic Roadmap

## Thesis

Runic is a Rust-native hosted allocator with a small auditable unsafe core,
out-of-line metadata, explicit ownership transitions, and tests that exercise
allocator invariants before performance layers hide bugs.

Runic is not a line-for-line port of mimalloc, jemalloc, TCMalloc, snmalloc, or
another allocator. It should learn from those allocators while keeping Runic's
domain model direct, Rust-native, and testable.

The useful claim is not:

```text
Runic is safe because it is written in Rust.
```

The useful claim is:

```text
Runic reduces and audits the unsafe core, encodes allocator invariants in
owned entities, and makes correctness measurable before adding concurrency,
hardening, or backend complexity.
```

Correctness comes before speed. Architecture should stay simple until a new
entity owns a real lifecycle, invariant, or policy.

## Current Status

Latest published release: `0.4.0`.

Current v0.5 development is an owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, lock-free remote-free inboxes, and Draining
lifecycle after thread exit, with explicit page-map ownership.

The current milestone is:

```text
TLS-owned heaps for small runs and large extents, HeapId ownership on entities,
claim→inbox→flush remote frees, Alloc miss flush-before-mmap, Free|Active|Draining
slot lifecycle, deterministic mapping retention, and randomized trace coverage.
```

## Supported Scope

Build only:

```text
Linux x86_64
Rust stable
GlobalAlloc
owner-local heaps via HeapTable / ThreadHeap
mmap-backed runs for size-classed allocations
mmap-backed extents for dedicated allocations (heap-local)
out-of-line metadata
page-indexed pointer lookup
per-size-class available run lists
per-block AtomicU8 run block state with remote-pending
lock-free remote-free Treiber inboxes per heap
configurable extent mapping retention and reuse
runs retained for the heap lifetime (no empty-run OS release in v0.5)
run block-boundary checks
extent exact-pointer checks
basic realloc
basic alloc_zeroed
randomized tests
```

Do not build yet:

```text
profiles
quarantine
canaries
hugepages
NUMA
C ABI
LD_PRELOAD
per-CPU caches
ML/lifetime placement
stats dashboard
```

## Core Invariants

```text
Every returned pointer maps to exactly one page-map entry.
Runs own one mapping and divide it into fixed-size reusable blocks from one size class.
Extents own one mapping dedicated to exactly one returned allocation.
Every free must map back to a known entry.
Run frees must be valid block boundaries.
Extent frees must be the exact returned pointer.
Cached mappings are not live allocations.
Cached blocks must have exactly one owner and must not be accepted as stale user frees.
```

If these invariants are wrong, thread-local heaps, remote frees, hardening, and
hugepage-aware allocation will hide correctness bugs. If they are right, the
allocator can be made faster without guessing.

## Architecture

Use this architecture first:

```text
GlobalAlloc
  -> RunicAlloc
      -> Allocator
          -> AllocatorInner { refs, pages: PageMap, table: Mutex<HeapTable> }
              -> HeapTable { generations[], slots: Arena<Heap> }
                  -> ThreadHeap
              -> Heap { RunHeap, ExtentHeap, Inbox }
                  -> RunHeap { Arena<Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

Keep one global lock around `HeapTable` for v0.5 slow paths; same-thread
small-run hits may use thread-owned heap metadata without entering that lock.
`PageMap` stays outside that mutex so dealloc lookup is not table-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API and abort boundary.
AllocatorInner owns the refcounted mmap instance: PageMap and Mutex<HeapTable>.
Heap           owns run and extent allocation policy for one heap identity.
HeapTable      owns slots Arena<Heap>, generations[], acquire/retire/reclaim, heap/mode, and publish.
Arena          owns fixed-capacity freelist metadata storage.
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       owns mmap and munmap.
PageMap        owns page-indexed owner-pointer lookup.
RunHeap        owns Arena<Run>, small-allocation policy, and available run lists.
Run            owns fixed-block allocation metadata and per-block state.
BlockStates    owns reusable, allocated, and remote-pending block state (one AtomicU8 per block).
ExtentHeap     owns Arena<Extent>, dedicated allocation policy, and mapping reuse.
ExtentCache    owns retained extent mappings, eviction, and reuse lookup.
Extent         owns dedicated allocation metadata.
```

Prefer direct methods on the entity that owns the state. Do not add passive
wrappers, compatibility shims, or test-only methods to production impl blocks.

## Workspace

```text
crates/runic-core
  allocator mechanics and core state; published as runic-core

crates/runic
  public GlobalAlloc wrapper; published as runic-alloc, imported as runic

crates/runic-test-support
  reusable test support; not published

crates/runic-bench
  Criterion, RSS, threaded, and policy-grid benchmark harnesses; not published
```

## Current Test Shape

Default tests should cover:

```text
layout normalization and overflow checks
size-class alignment invariants
per-block AtomicU8 block-state behavior
mmap mapping and writability
run block uniqueness and boundary checks
run arena reservation, insertion, mutation, removal
run cache retention and reuse policy
extent cache retention, eviction, and reuse policy
page-map lookup, removal, overlap rejection, L2 boundary crossing, spans
small and large allocation paths
alignment matrices
alloc_zeroed
realloc prefix preservation and in-place growth
subprocess abort cases
Box, Vec, String, HashMap, Arc smoke tests
deterministic randomized allocation traces
```

Abort tests must run in subprocesses, not inside the test harness process.

## Benchmark Policy

Use benchmarks to choose architecture, not to justify special cases.

Required checks for allocator-policy changes:

```text
cargo run -p runic-bench --release --bin policy_grid
cargo run -p runic-bench --release --bin rss -- --case runic large_alloc_churn_256k
cargo bench -p runic-bench --no-run
```

Use same-machine `perf stat` for page faults, branch behavior, and cache-miss
comparisons when a change affects mmap churn, page-map lookup, or hot small
allocation paths.

Current benchmark interpretation:

```text
Small allocation work is structurally limited by the global heap lock and shared
metadata. Do not add a global-lock block cache unless it improves hot paths and
preserves stale-free detection.

Dedicated extent churn is primarily controlled by mapping retention policy.
Keep extent retention deterministic, bounded, and allocation-free.

Empty-run OS release is not implemented in v0.5: runs stay published and arena-
resident for the heap lifetime. Extent retention policies are extent-only.
```

## Milestones

### v0.3 Released: Optimized Global-Lock Core

Delivered:

```text
out-of-line run and extent metadata
page-indexed owner lookup
available run lists
per-block AtomicU8 run block state
basic realloc and alloc_zeroed
randomized traces
abort-case tests
single-thread small allocation path improvements
```

Release artifacts:

```text
tag: 0.3.0
crates: runic-core 0.3.0, runic-alloc 0.3.0
```

### v0.4 Released: Retention Policy And Ownership Cleanup

Goal:

```text
Make mapping retention configurable, deterministic, bounded, and explicit while
keeping the global-lock architecture simple.
```

In scope:

```text
AllocatorConfig and ExtentConfig
ExtentPolicy and ExtentReuse
ExtentCache fixed-slot storage
policy_grid benchmark coverage
page-map publication/removal invariants for cached mappings
clear API documentation for policy and reuse semantics
```

Acceptance gate:

```text
workspace tests pass
workspace clippy passes with -D warnings
benchmark binaries build
policy_grid shows default behavior remains reasonable
RSS checks confirm bounded retention
runs remain retained by default (empty-run OS release not shipped)
```

### v0.5 Next: Full Thread-Local Heaps

Goal:

```text
Make same-thread small allocation hits avoid global metadata work while preserving
block-boundary, double-free, stale-free, remote-free, and thread-exit correctness.
```

In scope:

```text
HeapId ownership on Run and Extent (no Owner/root heap)
ThreadHeap frontend for small and large allocations
per-thread heap ownership through HeapTable slots
explicit block states for reusable, allocated, and remote-pending blocks
lock-free remote-free Treiber inbox on each Heap
alloc-miss flush then retry before mmap
thread-exit Draining mode with orphan flush and generation bump
heap-local extents
threaded benchmark reporting
```

Out of scope:

```text
NUMA
hugepages
adaptive cache sizing
hardening profiles
concurrent per-run remote freelists
per-CPU/RSEQ frontends
steal/adopt of live runs between heaps
```

Acceptance gate:

```text
same-thread local small allocation/free improves threaded churn
cross-thread frees remain validated and do not mutate owner freelists directly
thread exit with live allocations remains valid; late remote frees complete under Draining
existing abort tests pass
randomized cross-thread traces pass
no allocator-internal heap allocation is introduced
```

### v0.6 Later: Remote Free Queue Optimization

Goal:

```text
Optimize remote-free reuse latency after ownership and Draining are stable
(e.g. concurrent per-run remote freelists or freer-side batch buffers).
```

Acceptance gate:

```text
owner-side validation of every remote free remains mandatory
enqueue never drops frees and never blocks the freer on the owner
randomized cross-thread traces
abort cases remain intact
```

### v0.7 Later: Hardening

Goal:

```text
Strengthen corruption and misuse detection after ownership routing is explicit.
```

Order:

```text
checked or encoded reusable-block metadata
metadata cookies
optional delayed reuse
guard pages for selected large allocations
randomized placement only after deterministic paths are stable
```

### v0.8 Later: Backend Regions And Hugepage-Aware Allocation

Goal:

```text
Explore backend region ownership, decay, purge, and hugepage-aware mapping only
after mapping lifecycle and heap ownership are explicit.
```

## Reference Lessons

Use `allocator-refs/` as read-only inspiration:

```text
linked-list-allocator: minimal Rust GlobalAlloc shape and alignment tests
talc: Rust-native allocator structure and high-alignment regressions
ferroc: randomized allocation traces and zeroed allocation checks
mimalloc: page-local free-list and locality lessons
TCMalloc: frontend/middle/backend layering and size-class tests
snmalloc: remote-free/message-passing design
PartitionAlloc, Scudo, hardened_malloc: hardening and metadata boundaries
mimalloc-bench: workload and benchmark ideas
```

Do not copy reference implementation code.

## Standing Rules

```text
No backward compatibility is required for public or internal APIs.
Prefer reshaping existing APIs over adding parallel methods.
Keep names simple and domain-specific.
Keep allocator-internal caches allocation-free.
Do not add allocator-internal Vec, Box, HashMap, String, formatting, or panic paths
unless recursion risk is explicitly addressed.
Do not add thread-local heaps, remote frees, hardening, or hugepage support before
the milestone that owns the required invariants.
Track follow-up ideas in GitHub issues or focused docs, not as drive-by scope.
```
