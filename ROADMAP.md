# Ferralloc Roadmap

## Thesis

Ferralloc exists because Rust should have a serious Rust-native hosted allocator with a small auditable unsafe core, out-of-line metadata, explicit run invariants, and a clean path toward thread-local heaps, remote frees, hardening, and hugepage-aware allocation.

Ferralloc is not a line-for-line port of mimalloc, jemalloc, TCMalloc, snmalloc, or another allocator. It should learn from existing allocators while keeping Rust-native invariants explicit and testable.

The useful claim is not:

```text
Ferralloc is safe because it is written in Rust.
```

The useful claim is:

```text
Ferralloc reduces and audits the unsafe core, encodes allocator invariants explicitly, and makes allocator correctness testable before adding performance layers.
```

## Current Milestone

```text
A global-lock Rust allocator that can run real Rust programs and survive randomized allocation traces.
```

Correctness comes before speed.

## v0.1 Scope

Build only:

```text
Linux x86_64
Rust stable
GlobalAlloc
one global lock
mmap-backed runs for size-classed allocations
mmap-backed extents for dedicated allocations
out-of-line metadata
page-indexed pointer lookup
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

## Core Invariant

```text
Every returned pointer maps to exactly one page-map entry.
Runs own one mapping and divide it into fixed-size reusable blocks from one size class.
Extents own one mapping dedicated to exactly one returned allocation.
Every free must map back to a known entry: run frees must be valid block boundaries, and extent frees must be the exact returned pointer.
```

If this invariant is wrong, later features like thread-local heaps and remote frees will hide bugs. If it is correct, the allocator can be made fast later.

## Architecture

```text
GlobalAlloc
  -> Ferralloc
      -> Allocator
          -> Heap
              -> PageMap
              -> RunTable
              -> ExtentTable
              -> Run
                  -> FreeList
              -> Extent
              -> OsMemory
```

Use one global lock around `Heap`.

## Entity Responsibilities

```text
Ferralloc    owns the Rust GlobalAlloc boundary
Allocator    owns the core public allocator API
Heap         owns allocation policy and global lock-protected allocator data
LayoutSpec   owns normalized layout semantics
SizeClasses  owns size-class selection
OsMemory     owns mmap and munmap
Run          owns size-class fixed-block allocation metadata
Extent       owns dedicated allocation metadata
FreeList     owns the intrusive free-block chain
RunTable     owns out-of-line run metadata storage
ExtentTable  owns out-of-line extent metadata storage
PageMap      owns page-indexed pointer lookup
```

## Workspace

```text
crates/ferralloc-core
  allocator mechanics and global state

crates/ferralloc
  public GlobalAlloc wrapper

crates/ferralloc-test-support
  reusable future test machinery

crates/ferralloc-bench
  Criterion and RSS benchmark suite
```

## Reference Lessons

Use `allocator-refs/` as read-only inspiration:

- linked-list-allocator: minimal Rust `GlobalAlloc` shape, size/alignment matrix tests, free-order tests.
- talc: Rust-native allocator structure and high-alignment regression testing.
- ferroc: randomized allocation traces, fuzz-style action sequences, zeroed allocation checks, cookie validation.
- mimalloc: future run/page-local free-list design and locality lessons.
- TCMalloc: future frontend/middle/backend layering and size-class run invariant tests.
- snmalloc: future remote-free/message-passing design.
- PartitionAlloc, Scudo, hardened_malloc: later out-of-line metadata and hardening work.
- mimalloc-bench: later workload and benchmark ideas.

Do not copy reference implementation code.

## Current Test Shape

Default tests should cover:

```text
layout normalization and overflow checks
size-class alignment invariants
free-list LIFO behavior
mmap mapping and writability
run block uniqueness and boundary checks
run table reservation, insertion, mutation, removal
page map lookup, removal, overlap rejection, L2 boundary crossing
small and large allocation paths
alignment matrices
alloc_zeroed
realloc prefix preservation
subprocess abort cases
Box, Vec, String, HashMap, Arc smoke tests
deterministic randomized allocation traces
```

Abort tests must run in subprocesses, not inside the test harness process.

## Known Follow-Ups

Track these as GitHub issues instead of expanding v0.1 scope:

```text
Improve PageMap metadata allocation.
Add block-state tracking for double-free detection.
Revisit RunTable test/production capacity differences.
Add per-size-class available run lists.
Add extent reuse to avoid mmap/munmap churn.
Use profiling data to plan thread-local heap work.
```

## Profiling Notes

Current profiling says Ferralloc's next performance work should stay structural and
allocator-specific, not micro-optimized:

```text
small_biased_random, 2M ops:
  ferralloc ~208 ms
  snmalloc  ~127 ms
  mimalloc  ~173 ms
  system    ~254 ms

large_alloc_churn_256k, 100k ops:
  ferralloc ~346 ms, ~201k page faults, mostly sys time
  mimalloc  ~3.5 ms, ~203 page faults
  snmalloc  ~28 ms, ~155 page faults
  system    ~395 ms, ~300k page faults
```

Interpretation:

```text
Small allocation random traces are bottlenecked by RunTable fallback scans after
the active run misses. Add per-size-class available run lists before adding
thread-local heaps.

Dedicated extent churn is bottlenecked by mmap/munmap and page faults. Add a
bounded extent reuse policy before treating large-allocation benchmark results as
representative.

Threaded workloads are limited by the global Heap lock by design in v0.1. Use the
profiling data to shape, not rush, the later thread-local heap milestone.
```

## Later Milestones

```text
v0.2 block-state tracking
  Detect double frees and make block state explicit.

v0.3 available run lists
  Replace RunTable fallback scans with per-size-class available run tracking.

v0.4 extent reuse and release policy
  Reuse freed dedicated extents before returning mappings to the OS, with a simple bounded policy.

v0.5 thread-local heaps
  Add per-thread fast paths only after run invariants are stable.

v0.6 remote frees
  Add owner heap IDs and snmalloc/mimalloc-inspired remote handling.

v0.7 hardening
  Encoded freelists, canaries, quarantine, and guard-page ideas.

v0.8 hugepage-aware backend
  Explore 2 MiB segment packing and hugepage coverage later.
```
