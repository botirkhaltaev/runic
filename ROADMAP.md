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

The owner-local TLS magazine is that entity for v0.5. The leftover vs
snmalloc on this host (~1.55× on 64B churn) is not identity or take/refill
(#126/#128 skipped). The next entity is a per-CPU (or RSEQ) magazine — one
path, fail-closed DF, not a line-for-line port (`#135`, after `#129`).
Out-of-line metadata stays until Where shows an in-page run header is a ≥5%
lever.

## Current Status

Latest published release: `0.6.0`.

Current `master` ships the v0.6 owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, private run claim-bitmap remote admission, run/extent
`Inbox` coalesced by owner, and Draining lifecycle after thread exit, with explicit
page-map ownership. Heap lifecycle lives on `Heaps` / `Heap`
(Heaps indexes each Heap; each `Heap` owns inboxes and `RunHeap`/`ExtentHeap`).

Owner-local hit is a lockless TLS magazine (pop/push). `Run` is refill/`take` only.
#129 closeout on this host (`aa3a83a`, `compare_explicit` cycles/elem): 64-byte
churn 43.6 vs snmalloc 27.4 (**1.6×**). Isolated `owner_free` 62.4 vs mimalloc
13.6 (4.6×); `freelist` 44.6 vs snmalloc 16.3 (2.7×) — those phases pay
`take`/`allocate` at watermark 32. Large 64 KiB churn: Runic **best** (110 vs
mimalloc 133). Threaded local/4 is the same 1.6×; fan-in / ring are ~1.1–1.2×
snmalloc. #126/#128 skipped (identity / batch take not ≥5% levers).

The next milestone is:

```text
#135 per-CPU / RSEQ magazine vs this #129 baseline. Do not retry identity
or batch take. Do not raise the watermark.
```

## Supported Scope

Build only:

```text
Linux x86_64
Rust stable
GlobalAlloc
owner-local heaps via Heaps / ThreadHeap
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
ML/lifetime placement
stats dashboard
```

Next:

```text
per-CPU / RSEQ magazine (#135) — new entity, one hit, fail-closed DF
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
          -> AllocatorInner { refs, pages: PageMap, heaps: Heaps }
              -> Heaps { published[], arena: Mutex<Arena<Heap>>, config }
                  -> ThreadHeap
              -> Heap { HeapState, Inbox, id, RunHeap, ExtentHeap }
                  -> RunHeap { Arena<Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

`Heaps::get` / Active enqueue are lock-free via published pointers and
`HeapState` enqueue leases (lease before new `try_queue`). Arena mutex covers
acquire / Free reactivation only. Draining exclusivity is `LockedHeap` (not the
heaps arena mutex across flush). Shared `&Heap` is atomics-only; Active body
mutation is `ThreadHeap` only; reclaim is `LockedHeap` Drop only.
Same-thread small-run hits use TLS-owned heap metadata with no locks or atomics.
`PageMap` stays outside heaps arena locks so dealloc lookup is not heaps-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API, abort, and cold unbound routing.
AllocatorInner owns the refcounted mmap instance: PageMap, Heaps, and self-hosting Mapping.
Heaps           owns published heap pointers, lock-free get, arena acquire/reuse, and LockedHeap construction (`lock`).
Heap           owns HeapState, Inbox, and run/extent metadata; shared surface is atomics only (`enqueue` / mode).
LockedHeap     owns exclusive Draining body access to one Heap (flush / late free / reclaim on Drop).
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
ThreadHeap     owns TLS bind, per-class magazines, page→run cache, and the sole Active body path.
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
Active lease remote free and Draining late free
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
#129 matrix (this host, aa3a83a, compare_explicit cycles/elem):

phase/64        runic  snmalloc  mimalloc  jemalloc   vs best
owner_free       62.4      15.4      13.6      34.2    4.6× mi
freelist         44.6      16.3      22.5      30.0    2.7× sn
churn            43.6      27.4      33.4      32.1    1.6× sn

Runic churn is flat ~43–45 across 8/64/80/4096. owner_free 50.9 / 62.4 / 62.6 /
77.7. freelist 41.9 / 44.6 / 43.2 / 102.5 (4096 pays take).

large_alloc_churn/65536: runic 110, mimalloc 133, jemalloc 883, snmalloc 1283.
Runic wins on extent retention (Keep).

threaded/4: local 46.3 vs snmalloc 29.2 (1.6×). fan-in 392 vs 349 (1.1×).
ring 674 vs 576 (1.2×). Cross-allocator ratios are this-host Cost, not library drift.

Owner-local hit is magazine pop/push (no per-op `Run`). Leftover small-churn
cost is magazine links + TLS/`matches`. Isolated owner_free/freelist pay
`take`/`allocate`. Do not raise the watermark to hide them. #135 is the next
entity for the 1.6× churn gap.

Remote fan-in is close (run-coalesced Inbox). Use paired Runic cycles/op for
self-gates; use this table as the #135 competitor baseline.

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
per-thread heap ownership through Heaps / Heap / ThreadHeap
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

### v0.6 Released: Magazine hit and matrix closeout

Goal:

```text
After the magazine hit, only change owner-local identity or Run refill/take
when Where shows a ≥5% lever. Otherwise close the local matrix honestly.
```

Delivered (`#129` on this host, `aa3a83a`):

```text
magazine hit (#133): churn 75.0 → 43.6 (−42% vs H0)
#125 native TLS: skip (LocalKey % was inlined dealloc)
#126 identity: skip (~4% of churn; PageMap::get ~0%)
#128 batch take: skip (owner_free +53%, churn +29%)
#129 matrix: 1.6× snmalloc on churn/64; 4.6× mimalloc on owner_free/64;
             2.7× snmalloc on freelist/64; Runic best on large 64 KiB
             (110 vs mi 133); fan-in 1.1× / ring 1.2× snmalloc
API audit: allocate_fresh → bump; no sticky / *_v2 leftovers
```

Raw Cost lives under `target/runic-profiles/*id129*`. Watermark stays 32.
#135 uses this table as the competitor baseline.

Release artifacts:

```text
tag: 0.6.0
crates: runic-core 0.6.0, runic-alloc 0.6.0
```

### v0.7 Next: Per-CPU / RSEQ magazine

Goal:

```text
The leftover vs competitors after the TLS magazine is not identity or take.
A per-CPU (or RSEQ) magazine owns the next hit. One path. Fail-closed DF
and remote exact-once stay. Not a port of snmalloc.
```

Baseline: #129 closeout on this host. Issue: `#135`.

In:

```text
one CPU (or RSEQ) magazine entity
TLS magazine replaced or refilled in place (no dual hit)
watermark stays 32 until Where says otherwise
out-of-line Run metadata until a new ≥5% Where hit
```

Out:

```text
retry #126 / #128
dual ThreadHeap + CPU magazine hits
raising the watermark to hide take
in-page Run header without Where
hardening / hugepages (later)
```

Acceptance gate:

```text
≥5% vs #129 baseline on the phases the matrix names
rails ≤5% regress
record RSS / large / policy_grid
fmt, clippy -D warnings, cargo test --workspace
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

### v0.9 Later: Backend Regions And Hugepage-Aware Allocation

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
Do not add thread-local heaps, remote frees, per-CPU/RSEQ, hardening, or hugepage
support before the milestone that owns the required invariants.
Track follow-up ideas in GitHub issues or focused docs, not as drive-by scope.
```
