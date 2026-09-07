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

The owner-local current run is that entity for the small hit. The leftover vs
snmalloc / mimalloc on this host is instruction count on that hit (`locate` /
pop), not a magazine and not RSEQ. A per-CPU hit is a superset of the TLS hit on
pinned single-thread churn — `#135` Cost 70.1 → 69.3 → 65.3 vs the #129 TLS
magazine at 43.6 (gate ≤41.4). RSEQ is a footprint / thread-count lever, not a
hit lever. Do not retry per-CPU on single-thread churn. Owner DF is undefined;
remote admission stays fail-closed. Out-of-line metadata stays until Where shows
an in-page run header is a ≥5% lever.

## Current Status

Latest published release: `0.6.0`.

Current `master` ships the v0.6 owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, private run claim-bitmap remote admission, run/extent
`Inbox` coalesced by owner, and Draining lifecycle after thread exit, with explicit
page-map ownership. Heap lifecycle lives on `Heaps` / `Heap`
(Heaps indexes each Heap; each `Heap` owns inboxes and `RunHeap`/`ExtentHeap`).

Owner-local hit is a TLS current run per class (pop) plus a one-entry own-heap
run cache (`cache_base == usize::MAX` empty; probe `ptr - base < RUN_SIZE`).
`locate` is span + reciprocal divisibility (Lemire; one path, no jump table).
`Run::allocate` is pop only; `Run::extend` threads one page (min 32) of fresh
blocks on miss. Owner miss free is `Run::free` without Inner; `push_available`
only on `was_full`.

This pass owner-free hit diet vs post-#142 `82dd8b5` (same host, cycles/elem):

```text
                post142     diet     snmalloc
owner_free/64      15.3      11.1        11.5
freelist/64        20.4      20.4        18.9
churn/64           31.6      30.2        28.3
```

owner_free insn/elem 45.6 → 35.4 (sn 37.7); `free_slow` share 1.96% → <0.05%.
This diet owner_free is 11.1 vs snmalloc 11.5. Freelist still 1.08× snmalloc
(cycles flat; insn 58.7 → 65.3 is IPC, not a target).
`owner_free/4096` 30.7 cycles/elem; `recycled_churn/64/live:256` 57.2;
`programs/sh6bench/runic/1` 9488 (no post142 pair).

Locate diet vs run-local `75bb578` (same host, cycles/elem):

```text
                75bb578    locate
owner_free/64      22.0        16.9
freelist/64        19.0        18.5
churn/64           36.6        37.4
large 64 KiB      120.9       116.8
```

`compare_explicit` at `75bb578` (same host, cycles/elem):

```text
phase/64        runic  snmalloc  mimalloc  jemalloc   vs best
churn            36.6      27.8      36.0      34.5    1.31× sn
owner_free       23.0      14.3      13.6      36.1    1.69× mi
freelist         19.0      16.4      22.3      27.8    1.16× sn
large 64 KiB    120.9    1276.3     130.8     892.6    Runic best
```

Locate diet owner_free was 16.9 vs mimalloc 13.6 (~1.24×).

Criterion `compare_explicit` without `-C force-frame-pointers` (512-elem, ns/elem):
churn 7.46 vs sn 6.64 / mi 7.79; owner_free 4.43 vs sn 3.09 / mi 2.83; freelist 4.22 vs sn 4.02 / mi 4.35.
Profile Cost gates still include the ~3-instruction frame-pointer tax C allocators do not pay.

#129 closeout was 1.6× / 4.6× / 2.7× on those 64-byte phases. #126/#128 skipped.

`#135` RSEQ per-CPU: 65.3 vs 43.6 on churn/64, retired (not the lever).
O(1) TLS steal (this pass): freelist/64 18.5 → 22.9, gate missed; churn 37.4 → 31.3
did not save the AND. Seeded `freelist_allocate_only` pays steal per sample. Reverted.

Run-local `#140` vs `c1ecdeb` (this host):

```text
                c1ecdeb    run-local
churn/64           40.5         35.4
owner_free/64      74.0         22.2
freelist/64        37.3         18.5
large 64 KiB      133.3        116.1
local/4            43.7         36.8
fan-in/4            380          334
ring/4              676          663
```

owner_free / freelist beat their aims (25 / 22). Large recovered 133→116
(`#[cold]` audit) but missed ≤112.

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
pointer freelist + extend on runs (owner DF undefined)
private run claim-bitmap for remote admission (issued + try_set; no per-block Free byte)
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
Do not retry #126 / #128 / #135 (per-CPU on single-thread churn) / O(1) TLS steal
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
              -> Heaps { RwLock<Arena<Heap>>, free list, config }
                  -> ThreadHeap
              -> Heap { HeapState, Inbox, id, RunHeap, ExtentHeap }
                  -> RunHeap { Arena<Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

`Heaps::get` takes a short `RwLock` read to index `Arena<Heap>`, then returns
`&Heap` (occupied slots never move). Active enqueue uses `HeapState` leases
(lease before new `try_queue`). Write lock covers acquire / Free reactivation
only. Draining exclusivity is `Mutex<HeapInner>` via `Heaps::{enqueue,free,flush,reclaim}`
(not the arena lock across flush).
Shared `&Heap` is atomics-only; Active body mutation is `ThreadHeap` + `try_inner`;
reclaim is `Heap::reclaim` through `HeapsCtx`.
Same-thread small-run hits use TLS-owned heap metadata with no locks or atomics.
`PageMap` stays outside heaps arena locks so dealloc lookup is not heaps-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API, abort, and cold unbound routing.
AllocatorInner owns the refcounted mmap instance: PageMap, Heaps, and self-hosting Mapping.
Heaps           owns `RwLock<Arena<Heap>>`, Free-heap freelist, and Draining `enqueue` / `free` / `flush` / `reclaim`.
Heap           owns HeapState, Inbox, and `Mutex<HeapInner>`; shared surface is atomics only (`enqueue` / mode).
HeapInner      owns RunHeap / ExtentHeap (exclusive metadata).
HeapCtx        borrows PageMap for Active body ops.
HeapsCtx       borrows Heaps + PageMap for Draining reclaim.
Arena          owns grow-on-demand mmap slab storage (`vacant` / `insert` / `remove`; slots never move).
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       maps anonymous pages; Mapping owns the mmap lifecycle (Drop munmaps).
PageMap        owns page-indexed owner-pointer lookup.
RunHeap        owns Arena<Run>, run checkout (acquire), and available run lists.
Run            owns pointer freelist + extend + live, claim bitmap, and embedded InboxLink. Owner DF undefined.
ExtentHeap     owns Arena<Extent>, dedicated allocation policy, and mapping reuse.
ExtentCache    owns an intrusive head list of retained extents and exact-budget reuse.
Extent         owns dedicated allocation metadata, embedded InboxLink, and Claimed byte state.
ThreadHeap     owns TLS bind, current[class], own-heap run cache, and the sole Active body path.
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
  Criterion suites (micro / threaded / programs / global_*), metrics binary; not published
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
cargo run -p runic-bench --release --bin metrics
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

Owner-local hit is current-run pop / run-cache `Run::free` (`locate` + push).
Leftover small-churn cost is that hit's instruction count (`locate` / pop).
Isolated owner_free / freelist are the same path (no take). `#135` RSEQ is not
the lever — see thesis.

Remote fan-in is close (run-coalesced Inbox). Use paired Runic cycles/op for
self-gates; use this table as the competitor baseline.

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
ExtentCache intrusive head list, exact slot and byte budgets
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
`#135` RSEQ is retired (not the lever). This table stays the competitor baseline.

Release artifacts:

```text
tag: 0.6.0
crates: runic-core 0.6.0, runic-alloc 0.6.0
```

### v0.7 Next: Run-local hot list + hit diet

Goal:

```text
Delete the per-class TLS magazine. The small hit is a current run per class
plus a one-entry own-heap page cache. Diet the hit (TLS state byte, prologue,
cross-crate inlining, `#[cold]` audit). Remote exact-once stays. Owner DF is
undefined. Not a port of snmalloc. `#135` RSEQ is retired (not the lever).
```

Baseline: `c1ecdeb` on this host (churn/64 40.5, owner_free/64 74.0,
freelist/64 37.3).

In:

```text
ThreadHeap current[class] + own-heap-only page cache
Run::allocate pop-only; Run::extend threads one page (min 32)
unbind without take; UnbindGuard TLS; THREAD_HEAP no Drop
Allocator hit-only + #[inline] so RunicAlloc inlines
#[cold] only abort / bind / map / remote / unbind
```

Out:

```text
retry #126 / #128 / RSEQ / O(1) TLS steal
multi-entry page cache
Allocator singleton
in-page Run header without Where
hardening / hugepages (later)
```

Acceptance gate:

```text
churn/64 ≤ 38.5 (aim ≤ 33)
owner_free/64 ≤ 40 (aim ≤ 25)
freelist/64 ≤ 30 (aim ≤ 22)
large 64 KiB ≤ 112
threaded local/4, fan-in/4, ring/4 regress ≤ 5%
objdump: no callee-saved in Allocator::alloc/dealloc; RunicAlloc inlines
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
