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

Current `master` is ready for v0.5 planning. It has the v0.4 global-lock
allocator with configurable extent and empty-run retention policy work.

The current milestone is:

```text
A global-lock allocator with optimized single-thread small allocation paths,
stable run/extent metadata ownership, deterministic mapping retention policy,
and randomized trace coverage.
```

## Supported Scope

Build only:

```text
Linux x86_64
Rust stable
GlobalAlloc
one global heap lock
mmap-backed runs for size-classed allocations
mmap-backed extents for dedicated allocations
out-of-line metadata
page-indexed pointer lookup
per-size-class available run lists
bitmap-backed run block state
configurable extent mapping retention and reuse
optional empty-run release and mapping retention
run block-boundary checks
extent exact-pointer checks
basic realloc
basic alloc_zeroed
randomized tests
```

Do not build yet:

```text
profiles
thread-local heaps
remote frees
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
          -> Heap
              -> PageMap
              -> RunHeap
                  -> RunArena
                  -> RunCache
              -> ExtentHeap
                  -> ExtentArena
                  -> ExtentCache
              -> Run
                  -> FreeBitmap
              -> Extent
              -> OsMemory
```

Keep one global lock around `Heap` until the thread-local heap milestone.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API and abort boundary.
Heap           owns global allocation routing and shared allocator state.
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       owns mmap and munmap.
PageMap        owns page-indexed owner-pointer lookup.
RunHeap        owns small-allocation policy and per-class available run lists.
RunArena       owns out-of-line run metadata storage.
RunCache       owns retained empty-run mappings.
Run            owns fixed-block allocation metadata and block bitmap state.
FreeBitmap     owns block availability bits.
ExtentHeap     owns dedicated allocation policy and mapping reuse.
ExtentArena    owns out-of-line extent metadata storage.
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
bitmap block-state behavior
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

Empty-run release policies are opt-in. Early policy-grid runs show they can
regress single-size churn, so default behavior should keep empty runs live unless
new workloads justify a different default.
```

## Milestones

### v0.3 Released: Optimized Global-Lock Core

Delivered:

```text
out-of-line run and extent metadata
page-indexed owner lookup
available run lists
bitmap-backed run block state
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
AllocatorConfig, ExtentConfig, RunConfig
ExtentPolicy and ExtentReuse
RunPolicy for empty-run release experiments
ExtentCache and RunCache fixed-slot storage
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
empty-run release stays opt-in unless workloads justify changing the default
```

### v0.5 Next: Owner Identity For Local Heaps

Goal:

```text
Introduce the minimum owner identity needed for thread-local heaps and remote
free routing without adding local fast paths prematurely.
```

In scope:

```text
HeapId or equivalent non-zero owner identity
run or future region ownership metadata
page-map lookup that returns enough owner information for routing
thread-exit and cleanup design
tests for ownership lookup and invalid frees
```

Primary references:

```text
docs/span-ownership-evaluation.md
docs/thread-local-frontend-scope.md
docs/remote-free-protocol.md
```

### v0.6 Next: Thread-Local Small Allocation Frontend

Goal:

```text
Make same-thread small allocation hits avoid global metadata work while preserving
block-boundary, double-free, stale-free, and remote-free validation.
```

In scope:

```text
LocalHeap for small allocations only
fixed-size per-class local caches
batch refill from shared run metadata
validated local frees
bounded remote-free enqueue or validated fallback
threaded benchmark reporting
```

Out of scope:

```text
NUMA
hugepages
adaptive cache sizing
hardening profiles
unbounded queues
```

### v0.7 Later: Remote Free Protocol

Goal:

```text
Route cross-thread frees to the owning heap without allowing one thread to mutate
another thread's local metadata directly.
```

Acceptance gate:

```text
bounded allocation-free queue
owner-side validation of every remote free
queue-full behavior that does not drop frees
randomized cross-thread traces
abort cases remain intact
```

### v0.8 Later: Hardening

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

Primary reference: `docs/allocator-hardening-policy.md`.

### v0.9 Later: Backend Regions And Hugepage-Aware Allocation

Goal:

```text
Explore backend region ownership, decay, purge, and hugepage-aware mapping only
after mapping lifecycle and heap ownership are explicit.
```

Primary references:

```text
docs/span-ownership-evaluation.md
docs/hugepage-backend-exploration.md
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
