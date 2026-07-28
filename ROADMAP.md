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

Correctness is mandatory (auditable ownership, fail-closed frees). Within that,
performance is the primary design goal: keep hot paths simple and prefer safe
idiomatic Rust, with `unsafe` only where ownership/OS boundaries or measured
hot paths require it. Architecture should stay simple until a new entity owns a
real lifecycle, invariant, or policy.

## Current Status

Latest published release: `0.5.0`.

Current `master` ships the v0.5 owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, private run claim-bitmap remote admission, run/extent
`Inbox` coalesced by owner, and Draining lifecycle after thread exit, with explicit
page-map ownership. Heap lifecycle lives on `HeapDirectory` / `HeapSlot` (no thin
public `Heap` shell); run/extent metadata is private `SlotHeap` inside each slot.

Local free/churn hot paths use claim-bitmap run free (no owner `lock cmpxchg`) and
owner-coalesced remote publication. Measured on Linux x86_64 (paired Runic cycles/op
vs pre-claim-bitmap baseline): 64-byte owner_free −23%, churn −12%, fan-in −35%.
Small local churn remains ~2.7× snmalloc on the same host (TLS + metadata residue;
not parity).

The next milestone is:

```text
Reduce TLS entry and page-map lookup overhead on owner-local alloc/dealloc without
weakening fail-closed ownership or multi-allocator thread safety.
```

## Supported Scope

Build only:

```text
Linux x86_64
Rust stable
GlobalAlloc
owner-local heaps via HeapDirectory / ThreadHeap
mmap-backed runs for size-classed allocations
mmap-backed extents for dedicated allocations (heap-local)
out-of-line metadata
page-indexed pointer lookup
per-size-class available run lists
per-block AtomicU8 clear/Free on runs (Free bit DF fail-closed; freelist+bump own Free/Live)
private run claim-bitmap for remote admission (no byte Claimed on runs)
run/extent Inbox coalesced by owner (Treiber stack of runs/extents, not per-block nodes)
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
          -> AllocatorInner { refs, pages: PageMap, directory: HeapDirectory }
              -> HeapDirectory { published[], state: Mutex<Arena<HeapSlot>> }
                  -> ThreadHeap
              -> HeapSlot { SlotState, Inbox, SlotHeap{id, RunHeap, ExtentHeap} }
                  -> RunHeap { Arena<Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

`HeapDirectory::slot` / Active enqueue are lock-free via published pointers and
SlotState publisher leases. Acquire, retire, Draining accept/free, and reclaim take the private
directory lifecycle mutex. Same-thread small-run hits use TLS-owned heap metadata.
`PageMap` stays outside that mutex so dealloc lookup is not directory-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API, abort, and cold unbound routing.
AllocatorInner owns the refcounted mmap instance: PageMap, HeapDirectory, and self-hosting Mapping.
HeapDirectory  owns published slot pointers, lock-free lookup/Active enqueue, and locked acquire/retire/lock→LockedSlot/reclaim.
HeapSlot       owns SlotState (gen+mode+retired+publishers), Inbox, and run/extent metadata (RunHeap + ExtentHeap).
Arena          owns fixed-capacity freelist metadata storage.
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       maps anonymous pages; Mapping owns the mmap lifecycle (Drop munmaps).
PageMap        owns page-indexed owner-pointer lookup.
RunHeap        owns Arena<Run>, run checkout (acquire), and available run lists.
Run            owns fixed-block allocation metadata, freelist-primary Free/Live, bump, and embedded InboxLink.
BlockStates    owns clear/Free per-block bytes (one AtomicU8 per block); Free bit is DF fail-closed, not Free/Live authority.
ExtentHeap     owns Arena<Extent>, dedicated allocation policy, and mapping reuse.
ExtentCache    owns retained extent mappings, eviction, and reuse lookup.
Extent         owns dedicated allocation metadata, embedded InboxLink, and Claimed byte state.
ThreadHeap     owns TLS bind, sticky runs, and page→run cache.
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
page-map lookup, removal, overlap rejection, L2 boundary crossing
small and large allocation paths
alignment matrices
alloc_zeroed
realloc prefix preservation and in-place growth
subprocess abort cases
Box, Vec, String, HashMap, Arc smoke tests
deterministic randomized allocation traces
Active publisher-lease remote free and Draining late free
thread-exit / never-bound freer claim→enqueue (no TLS batch)
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
Owner-local run free no longer uses lock cmpxchg (claim-bitmap handshake). Remaining
small-churn cost is mostly TLS entry (`LocalKey::with`) and page-map lookup, not
per-block byte CAS.

Remote fan-in improved via run-coalesced Inbox publication; cross-allocator ratios
are informational (library/host drift) — use paired Runic cycles/op for PR gates.

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
ExtentPolicy::{Drop, Keep} with exact-length reuse
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

### v0.5 Released: Full Thread-Local Heaps

Delivered:

```text
HeapId ownership on Run and Extent (no Owner/root heap)
ThreadHeap frontend for small and large allocations
per-thread heap ownership through HeapDirectory / HeapSlot
explicit block states for reusable and allocated run blocks; extent Claimed
run/extent Inbox coalesced by owner (claim → enqueue → accept)
private run claim-bitmap remote admission (owner free store/recheck; no owner lock cmpxchg)
alloc-miss prefers local/OS run acquire, then flush+retry before mmap
thread-exit Draining mode with orphan flush and generation bump
heap-local extents
freelist-primary run allocate/free and page-map atomic publish
grow-on-demand metadata arenas
single Allocator::abort sink
threaded benchmark reporting and local profile.sh
```

Release artifacts:

```text
tag: 0.5.0
crates: runic-core 0.5.0, runic-alloc 0.5.0
```

### v0.6 Next: Owner-local TLS and lookup overhead

Goal:

```text
Reduce TLS entry and page-map lookup cost on owner-local alloc/dealloc while
preserving fail-closed ownership, multi-allocator thread safety, and the
claim → enqueue → accept protocol.
```

Acceptance gate:

```text
≥5% improvement on phase-isolated owner_free and single_size_churn vs paired baseline
≤3% regression on unaffected matrix rows
owner-side validation of every remote free remains mandatory
randomized cross-thread traces and abort cases remain intact
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
