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

The owner-local current run is that entity for the small hit. Leftover vs
snmalloc on this host is `global_*` collection Cost (`vec_many_small` 1.71×,
then `word_count` / `http_buffers` ~1.10×, `json_api` / `regex_search` ~1.07×,
`tree`). Do not retry a magazine, RSEQ, or a locate-offset dual free. Owner
DF is undefined; remote admission stays fail-closed. One process-wide payload;
TLS identity is free. Out-of-line metadata stays until Where shows an in-page
run header is a ≥5% lever.

## Current Status

Latest published release: `0.6.0`.

Current `master` ships the v0.6 owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, private run claim-bitmap remote admission, run/extent
`Inbox` coalesced by owner, and Draining lifecycle after thread exit, with explicit
page-map ownership. Heap lifecycle lives on `Heaps` / `Heap`
(Heaps indexes each Heap; each `Heap` owns inboxes and `RunHeap`/`ExtentHeap`).

Owner-local hit is a TLS current run per class (pop) plus a one-entry own-heap
`RunCache`. `locate` is offset from the run base. Run mappings are
`RUN_SIZE`-aligned. `Run::allocate` is pop only; `extend` on miss. Owner free
is `Run::free`; `push_available` only on `was_full`. One process-wide payload;
`Allocator::ctx()` is the handle.

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

Small-hit instruction diet vs `7064ab6` same-session (this host, cycles/elem):

```text
                7064ab6     diet     snmalloc
owner_free/64      12.5      11.9        11.5
freelist/64        21.2      20.4        19.0
churn/64           30.2      29.4        28.0
```

`class_for` default-align indexes by size (drop `size.max(align)`).
`Allocator::push_available` hides TLS from the free hit. `locate` uses the run
base; a locate-offset dual `free` / `free_at` is out of scope.
Large 64 KiB same-session: 136.7 vs mi 133.3 (Keep).

Hit reshape vs `7064ab6` diet (same host, cycles/elem; this branch):

```text
                diet    singleton    folds    aligned     snmalloc
owner_free/64   11.9         11.0    10.4       9.4         11.5
freelist/64     20.4         20.4    20.0        —          19.0
churn/64        29.4         28.2    28.0      27.5         28.0
```

Singleton drops TLS `matches`. Folds: `dealloc` forbids null; `CLASS_FOR_SIZE[0]`
+ raw `LayoutSpec` size; one-branch admission. Aligned runs keep `locate` on the
run base. `large_churn/65536` 143.4 (Criterion: no change vs prior binary).
owner_free and churn beat last same-host snmalloc Cost; leftover then was
freelist (now `global_*` collection Cost).

Available-list leak: `push_available` was not idempotent. A full current run
pushed twice linked `A.next = A` and dropped the tail, so `acquire` mmapped
forever (small_biased_random: 957k faults / 1.2 M/s vs snmalloc 363 / 10.1 M/s).
`RunState.on_available` (after `free`, off the hit) + unbind returning current
runs. Guard vs `*leakfix-fp*` / prior aligned: owner_free 9.41 (was 9.56),
churn 27.5 (was 27.7), freelist 21.0 (was 20.0).

Extent default budget 32 slots / 16 MiB → 64 / 64 MiB (exact-length reuse).
First-fit `want <= have <= 2 * want` regressed sh6bench (32 K/s, 1.2M faults)
and was not kept.

Tier 2 Cost vs snmalloc after the leak fix (this host, cycles/elem; `profile.sh`
`cycles:u` plus `page-faults` / bash `time` so kernel time cannot hide):

```text
phase                         runic      sn      ×sn   faults r/sn     elems/s r/sn    notes
sh6bench/1                    5776    10066     0.57   303k / 330      409k / 372k     after 64/64 MiB budget
size_boundary_sweep           47.9     49.1     0.97   —               79.5M / 77.1M   L1d 1.8% vs 6.3%
small_biased_random            262      372     0.70   1343 / 359      14.4M / 10.1M   was 571 / 1.2M/s
remote_fan_in/4/live:256      7619     4055     1.88   518 / 382       922k / 778k     noisy; r elems/s higher
free_ring/4/live:256          2081     1613     1.29   428 / 348       689k / 721k     same shape
```

`profile.sh` is user-cycles only. Always pair it with `page-faults` + `time`
sys on mixed/large benches. First-fit extent reuse and lock-free `Heaps::get`
were measured and not taken.

`CLASS_FOR_SIZE` is not the mixed-size lever: sweep is tied and runic L1 is
*lower* there. Compact class tables are not the next change.

Alloc hit (`RunicAlloc::alloc` objdump): default-align is `CLASS_FOR_SIZE` +
TLS current pop + `live++`. No leftover fold. Freelist residual is 21.0 vs 19.0.

Fan-in leaf split (`*where-runic*…fan_in*`, 711k samples): bench 50.9%;
`free_fail` 16.5% (annotate: second `PageMap::get` — 54% `test` L1, then L2
load); `free_remote` 16.5% (inlined `claim` `lock bts` 14%, `Heaps::get` /
`locate` cmps); `Heap::enqueue` 2.2%; `flush` 2.8%; `free_slow` 2.3%.
Kept `ThreadFreeError::Remote(PageOwner)` (drop the second get): fan-in
5296 (−4.7%, +30% elems/s) vs ring 2088 (+2.6%). Fan-in is the named path.
Owner-local guard vs `*aligned*`: churn +0.8%; owner_free +1.9% cycles /
+0.8% insn (`Remote` is not on that hit).

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

Criterion without `-C force-frame-pointers` (512-elem, ns/elem):
churn 7.98 vs sn 6.66 / mi 8.95; owner_free 3.92 vs sn 3.11 / mi 3.55; freelist 4.49 vs sn 4.48 / mi 5.25.
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
heap-owned 2 MiB run maps (16 spaces) for size-classed allocations
mmap-backed extents for dedicated allocations (heap-local)
out-of-line metadata
page-indexed pointer lookup (run publish is the 64 KiB payload only)
per-size-class available run lists
pointer freelist + extend on runs (owner DF undefined)
private run claim-bitmap for remote admission (issued + try_set; no per-block Free byte)
run/extent Inbox coalesced by owner (Treiber stack of runs/extents, not per-block nodes)
configurable extent mapping retention and reuse
runs retained for the heap lifetime; Discard is madvise on empty payload
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
/ locate-offset dual free / TLS extent cache / first-fit extent reuse
/ lock-free Heaps::get
```

## Core Invariants

```text
Every returned pointer maps to exactly one page-map entry.
A run owns one size class and one range in a heap-owned map.
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
      -> Allocator          // const handle; ctx() borrows Process
          -> Process { pages: PageMap, heaps: Heaps }  // mmap; not returned
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
reclaim is `Heap::reclaim` through `Heaps`. `Allocator::ctx()` is the only
handle into the process payload. Same-thread small-run hits use TLS-owned heap
metadata with no locks or atomics. `PageMap` stays outside heaps arena locks so
dealloc lookup is not heaps-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API, abort, and cold unbound routing.
AllocatorCtx   borrows PageMap + Heaps for miss / bind / unbind / body / Draining.
Process        owns the process-wide mmap payload (PageMap + Heaps); not returned.
Heaps           owns `RwLock<Arena<Heap>>`, Free-heap freelist, and Draining `enqueue` / `free` / `flush` / `reclaim`.
Heap           owns HeapState, Inbox, and `Mutex<HeapInner>`; shared surface is atomics only (`enqueue` / mode).
HeapInner      owns RunHeap / ExtentHeap (exclusive metadata).
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
ThreadHeap     owns TLS bind, current[class], RunCache, and the sole Active body path.
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
  Criterion `global_*` (collections + serde_json / regex / bytes), metrics binary; not published
```

## Current Test Shape

Default tests should cover:

```text
layout normalization and overflow checks
size-class alignment invariants
run freelist / claim-bitmap / locate behavior
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

Owner-local hit is current-run pop / `lookup` then `Run::free` (locate + push).
Leftover vs snmalloc is `global_*` collection Cost. Same-session maps reshape
(`profile.sh` cycles/elem vs snmalloc): `vec_many_small` 64.8 / 37.9 (1.71×;
Where is alloc/dealloc — `lookup`, TLS `with`, `Run::free`); `word_count`
365 / 331 (1.10×; fmt/hash); `http_buffers` 232 / 211 (1.10×; `bytes`);
`json_api` 6400 / 5984 (1.07×; btree); `regex_search` 1123 / 1047 (1.07×;
teddy). `#135` RSEQ is not the lever — see thesis.

Remote fan-in is close (run-coalesced Inbox). Use paired Runic cycles/op for
self-gates; use this table as the competitor baseline.

Dedicated extent churn is primarily controlled by mapping retention policy.
Keep extent retention deterministic, bounded, and allocation-free.

Empty-run `Discard` is opt-in (`madvise` on the payload). Maps stay; runs stay
published and arena-resident. Default is `Keep` until Cost says otherwise.
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
