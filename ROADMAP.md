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

The owner-local current run is that entity for the small hit. LTO collection
Cost on this host puts `vec_many_small` at 30.7 vs snmalloc 33.2 (0.93×).
App-bound cases (`tree`, `http_buffers`, …) stay near parity. Substantial
wins vs snmalloc live on remote-free, large buffers, and footprint — see
Benchmark Policy. Do not retry a magazine, RSEQ, or a locate-offset dual
free. Owner DF is undefined; remote admission stays fail-closed. One
process-wide payload; TLS identity is free. The `Run` header lives in the
run space (`base + RUN_SIZE`); small free is `header_of` (mask + `base` check).

## Current Status

Latest published release: `0.6.0`.

Current `master` ships the v0.6 owner-local heap frontend: TLS heaps own runs and
extents stamped with `HeapId`, private run claim-bitmap remote admission, run/extent
`Inbox` coalesced by owner, and Draining lifecycle after thread exit, with explicit
page-map ownership. Heap lifecycle lives on `Heaps` / `Heap`
(Heaps indexes each Heap; each `Heap` owns inboxes and `RunHeap`/`ExtentHeap`).

Owner-local hit is a TLS current run per class (pop). Small miss/realloc uses
`Run::header_of`. `locate` is offset from the run base. Run mappings are
`RUN_SIZE`-aligned; the header and claim tail sit after the payload. `Run::allocate`
is pop only; `extend` on miss. Owner free is `Run::free`; `push_available` only
on `was_full`. One process-wide payload; `Allocator::ctx()` is the handle.
A Draining heap may be `adopt`ed by the first remote freer (`Draining` → `Active`).

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
sys on mixed/large benches. First-fit extent reuse was measured and not taken.
`Heaps::get` is a lock-free `Arena` read.

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

`#135` RSEQ per-CPU: 65.3 vs 43.6 on churn/64. Impl never reached the
tcmalloc hit; the gate was single-thread churn. Retired as a runic hit.
Primitive work: [crates/rseq-rs/ROADMAP.md](crates/rseq-rs/ROADMAP.md).
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
Rust nightly (`#[thread_local]` `THREAD_HEAP`)
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
              -> Heaps { Arena<Heap>, free list, config }
                  -> ThreadHeap
              -> Heap { HeapState, Inbox, Mutex<HeapInner> }
                  -> HeapInner { id, RunHeap, ExtentHeap }
                  -> RunHeap { Arena<Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

`Heaps::get` is a lock-free `Arena` read: `len` Acquire, chunk pointer
Acquire, then `state.matches`. Occupied slots never move. Active enqueue uses
`HeapState` leases (lease before new `try_queue`). Arena grow covers
mapping ownership and bump insert only. Draining exclusivity is
`Mutex<HeapInner>` via `Heaps::{enqueue,free,flush,reclaim}`
(not the arena grow lock across flush).
Shared `&Heap` is atomics-only; Active body mutation is `ThreadHeap` + `require_inner`;
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
Heaps          owns `Arena<Heap>`, the Free-heap freelist, and Draining `enqueue` / `free` / `flush` / `reclaim`.
Heap           owns HeapState, Inbox, and `Mutex<HeapInner>`; shared surface is atomics only (`enqueue` / mode).
HeapInner      owns RunHeap / ExtentHeap (exclusive metadata).
Arena          owns published immovable slots (`get` lock-free; `push` shared; `vacant` / `insert` / `remove` exclusive).
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       maps anonymous pages; Mapping owns the mmap lifecycle (Drop munmaps).
PageMap        owns page-indexed owner-pointer lookup.
RunHeap        owns Arena<NonNull<Run>> (in-space headers), run checkout (acquire), and available run lists.
Run            owns in-page header, pointer freelist + extend + live, claim bitmap, and embedded InboxLink. Owner DF undefined.
ExtentHeap     owns Arena<Extent>, dedicated allocation policy, and mapping reuse.
ExtentCache    owns an intrusive head list of retained extents and exact-budget reuse.
Extent         owns dedicated allocation metadata, embedded InboxLink, and Claimed byte state.
ThreadHeap     owns TLS bind, current[class], at most one adopted heap, and the sole Active body path.
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
Competitor crates (runic-bench defaults; no extra features):
  snmalloc-rs 0.3.8  cmake Release -O3, initial-exec TLS, wait-on-address.
                     native-cpu off (matches rustc x86-64, not -march=native).
                     crate `lto` feature is a no-op on the Linux cmake path.
  mimalloc 0.1.52    v3, MI_SECURE off, initial-exec TLS.
  jemalloc 0.6.1     background_threads_runtime_support only (no background threads).

profile.sh Cost is the ordinary bench binary (no forced frame pointers).
Where uses LBR. v0 symbol mangling stays on. Pair:
  vec_many_small runic  fp 42.7 / no-fp 44.8 (fp fewer cycles, more insns).
  Dropped the force so Cost matches `cargo bench` and C omit-fp.

[profile.bench] lto = "fat", codegen-units = 1 (adopted):
  vec_many_small  runic 44.8 → 32.6 (−27%); snmalloc 39.4 → 33.2 (−16%).

After cold `maybe_discard` + Run hot-field pack (base/span/recip +
free/live/capacity): vec_many_small 30.7. Gate ≤ 40. vs LTO snmalloc 33.2
(0.93×). objdump: `__rust_alloc` has no callee-saved; `Allocator::dealloc`
still pushes rbx/r14/r15 for was_full / Discard. `__rust_dealloc` is a jmp.
Realloc multi-entry cache skipped. One-entry `RunCache` deleted; small
miss/realloc is `Run::header_of`.

#129 synthetic matrix stays the historical competitor baseline (aa3a83a).
Collection leftover vs snmalloc is app work (`tree` / `http_buffers` ~1.01×
pre-LTO).

Track C Cost (this host, LTO, `RUNIC_PROFILE_CPUS=0-3` for threaded;
same-session after adopt + in-page header + lazy-zero ≥256 KiB):

  workload            runic     snmalloc   mimalloc    vs sn
  vec_many_small      15.8        33.4      34.5     0.47×
  json_api            4608        4762      4774     0.97×
  channel_pipeline    1071        1469      1965     0.73×
  arc_share_drop       260         313       402     0.83×
  scoped_map_reduce    595         544       670     1.09×
  large_buffers       3576        4921     67425     0.73×

`arc_share_drop` is the remote last-drop win. `channel_pipeline` is the
adopt + mask-lookup win. Mixed-size 64 KiB–1 MiB `large_buffers` under
default Keep now discards on Zeroed reuse ≥256 KiB (0.73× sn); dirty
touch-every-page remains a memset/fault tax, not a missing medium class.
Switch-on-second-heap adopt lost on `channel_pipeline` (sticky + empty
reclaim kept). Bitmap `accept` was not tried (`flush`/`accept` <4% after
adopt).

Keep vs Discard vs Unmap on `large_buffers` (metrics `--case`, 64 ops,
`perf stat -r 5` cycles:u; same-host):

  policy              cycles     minflt   rss_after
  Keep (default)     7.60M       2186     21.5 MiB
  Discard            1.64M        141     12.5 MiB
  Unmap              1.69M        132     12.6 MiB
  snmalloc           1.71M        121     12.4 MiB
  mimalloc           6.28M          6     24.6 MiB

Discard (retain mapping, `madvise`, skip memset when advise succeeds)
matches snmalloc. Unmap is the same order. Keep and mimalloc pay the
dirty memset. Medium size-classes are not the lever. Default stays
Keep (dirty reuse). `runic:discard/keep` is the opt-in.

RSS / minflt (`metrics` bin; syscall tracepoints not permitted on this host):

  workload            runic peak/after/flt     snmalloc
  vec_many_small      12.6 / 12.6 / 23         12.2 / 12.2 / 0
  http_buffers        12.6 / 12.6 / 5          12.3 / 12.3 / 1
  channel_pipeline    13.4 / 13.4 / 163        12.5 / 12.5 / 29
  arc_share_drop      13.2 / 13.2 / 123        12.5 / 12.5 / 25
  scoped_map_reduce   13.0 / 13.0 / 90         12.4 / 12.4 / 26
  large_buffers       21.5 / 21.5 / 2188       12.7 / 12.4 / 121

Runic `Keep` holds large mappings (rss_after_free == peak). That is policy,
not a hack. jemalloc is smallest on `large_buffers` (9.9 / 9.0 / 120).

Dedicated extent churn is primarily controlled by mapping retention policy.
Keep extent retention deterministic, bounded, and allocation-free.

Empty-run `Discard` is opt-in (`madvise` on the payload). Maps stay; runs stay
published and arena-resident. Default is `Keep` until Cost says otherwise.

Draining remote-free Cost after lock-free `Heaps::get` + reclaim gate
(runic vs the same dirty tree before the change, `scripts/profile.sh`
5s/5rep, this host — not vs snmalloc; Track C still has the last
same-session competitor Cost):

  workload            before    after    vs before
  scoped_map_reduce     613      387     0.63×  (≤400 gate)
  arc_share_drop        412      321     0.78×
  channel_pipeline      912      680     0.75×
  large_buffers       77706    77531     unchanged

`Heaps::get` is inlined (two Acquire loads). Where no longer shows the
RwLock reader CAS. `Heap::reclaim` scans only when a Draining free emptied
its owner. `admit` is one directory `get`, then a generation/mode recheck
after the Inner lock.

Policy grid (`scripts/policy_grid.sh`, N=5, train/hold-out): no candidate
beat Keep/Keep by ≥5% geomean without a hold-out or train regression.
`runic:discard/keep` wins zeroed `large_buffers` (0.27M vs 5.0M ns) and
fails `large_buffers_dirty` (2.3×). Run `Discard` is 2–8× on small
churn. Extent/run Bound candidates were measured and not landed. Default
stays Keep/Keep. `Unmap` remains the unretained baseline.
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
ExtentPolicy::{Keep, Discard, Unmap} with exact-length reuse
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
#125 native TLS: skip then (LocalKey % was inlined dealloc); reopened as
             #[thread_local] THREAD_HEAP after dealloc LocalKey::with showed as
             the vec_many_small leaf
#126 identity: skip (~4% of churn; PageMap::get ~0%)
#128 batch take: skip (owner_free +53%, churn +29%)
#129 matrix: 1.6× snmalloc on churn/64; 4.6× mimalloc on owner_free/64;
             2.7× snmalloc on freelist/64; Runic best on large 64 KiB
             (110 vs mi 133); fan-in 1.1× / ring 1.2× snmalloc
API audit: allocate_fresh → bump; no sticky / *_v2 leftovers
```

Raw Cost lives under `target/runic-profiles/*id129*`. Watermark stays 32.
`#135` RSEQ is retired as a runic hit on single-thread churn. This table
stays the competitor baseline. See [crates/rseq-rs/ROADMAP.md](crates/rseq-rs/ROADMAP.md).

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
undefined. Not a port of snmalloc. `#135` RSEQ is retired as a runic hit
on single-thread churn. Primitive: [crates/rseq-rs/ROADMAP.md](crates/rseq-rs/ROADMAP.md).
```

Baseline: `c1ecdeb` on this host (churn/64 40.5, owner_free/64 74.0,
freelist/64 37.3).

In:

```text
ThreadHeap current[class] + own-heap-only page cache
Run::allocate pop-only; Run::extend threads one page (min 32)
unbind without take; UnbindGuard LocalKey; #[thread_local] THREAD_HEAP (#125)
Allocator hit-only + #[inline] so RunicAlloc inlines
#[cold] only abort / bind / map / remote / unbind / discard / adopt
```

Out:

```text
retry #126 / #128 / RSEQ / O(1) TLS steal
multi-entry page cache
switch-on-second-heap adopt (lost on channel_pipeline)
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

## Related: rseq-rs

Standalone librseq-in-Rust word ops (not a runic hit). Thesis and releases
live in [crates/rseq-rs/ROADMAP.md](crates/rseq-rs/ROADMAP.md). v0.1 is in
progress on `rseq/*` branches. Do not wire it into the allocator hit from
this roadmap.

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
