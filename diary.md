# Runic Measurement Diary

Session-local Cost, Where, RSS, and experiment verdicts. Not current scope.
See [ROADMAP.md](ROADMAP.md) for milestones and [ARCHITECTURE.md](ARCHITECTURE.md)
for design.

Host notes below are one Linux x86_64 box unless said otherwise. Cycles are
`scripts/profile.sh` user cycles unless a table says Criterion midpoint.

## Do not retry

Measured and declined or reverted:

```text
#126 identity
#128 batch take
#135 RSEQ as a runic hit (single-thread churn)
per-CPU heaps on rseq-rs 0.8 (Phase 0)
O(1) TLS steal
locate-offset dual free
magazine (replaced by current-run)
multi-entry page cache
TLS extent cache
first-fit extent reuse
compact CLASS_FOR_SIZE
switch-on-second-heap adopt (lost on channel_pipeline)
multi-slot adopt (lost on channel_pipeline)
reclaim live-scan elimination
realloc known-owner reuse
spawn_churn-only fault package (one-page extend, lazy Arena, front-header, …)
snmalloc port
per-run deferred remote list (`thread_free`)
BatchIt-on-Inbox (last-run skip and bounded four-way delayed cache)
heap-local available fullness bins
cross-heap empty-run restamp / abandon pool
2 MiB-aligned maps without over-map trim
owner-local empty-run DONTNEED on unbind
128-slot / 128 MiB extent budget
```

## Hit diet and run-local

Owner-free hit diet vs post-#142 `82dd8b5` (cycles/elem):

```text
                post142     diet     snmalloc
owner_free/64      15.3      11.1        11.5
freelist/64        20.4      20.4        18.9
churn/64           31.6      30.2        28.3
```

owner_free insn/elem 45.6 → 35.4 (sn 37.7); `free_slow` share 1.96% → <0.05%.

Small-hit instruction diet vs `7064ab6`:

```text
                7064ab6     diet     snmalloc
owner_free/64      12.5      11.9        11.5
freelist/64        21.2      20.4        19.0
churn/64           30.2      29.4        28.0
```

Hit reshape vs that diet:

```text
                diet    singleton    folds    aligned     snmalloc
owner_free/64   11.9         11.0    10.4       9.4         11.5
freelist/64     20.4         20.4    20.0       n/a         19.0
churn/64        29.4         28.2    28.0      27.5         28.0
```

Singleton drops TLS `matches`. Folds: `dealloc` forbids null; `CLASS_FOR_SIZE[0]`
+ raw `LayoutSpec` size; one-branch admission. Aligned runs keep `locate` on the
run base.

Available-list leak: `push_available` was not idempotent. A full current run
pushed twice linked `A.next = A` and dropped the tail. Fix:
`RunState.on_available` (after `free`, off the hit) + unbind returning current
runs.

Extent default budget 32 slots / 16 MiB → 64 / 64 MiB (exact-length reuse).
First-fit `want <= have <= 2 * want` regressed sh6bench and was not kept.

Run-local `#140` vs `c1ecdeb`:

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

`class_for` default-align indexes by size (drop `size.max(align)`).
`Allocator::push_available` hides TLS from the free hit. Compact `CLASS_FOR_SIZE`
is not the mixed-size lever.

## Locate and magazine closeout

Locate diet vs run-local `75bb578`:

```text
                75bb578    locate
owner_free/64      22.0        16.9
freelist/64        19.0        18.5
churn/64           36.6        37.4
large 64 KiB      120.9       116.8
```

`compare_explicit` at `75bb578`:

```text
phase/64        runic  snmalloc  mimalloc  jemalloc   vs best
churn            36.6      27.8      36.0      34.5    1.31× sn
owner_free       23.0      14.3      13.6      36.1    1.69× mi
freelist         19.0      16.4      22.3      27.8    1.16× sn
large 64 KiB    120.9    1276.3     130.8     892.6    Runic best
```

Criterion without `-C force-frame-pointers` (512-elem, ns/elem): churn 7.98 vs
sn 6.66 / mi 8.95; owner_free 3.92 vs sn 3.11 / mi 3.55; freelist 4.49 vs sn 4.48
/ mi 5.25.

`#129` closeout was 1.6× / 4.6× / 2.7× on those 64-byte phases. `#126` / `#128`
skipped. Raw Cost lives under `target/runic-profiles/*id129*`.

`#135` RSEQ per-CPU: 65.3 vs 43.6 on churn/64. Never reached the tcmalloc hit;
retired as a runic hit. O(1) TLS steal: freelist/64 18.5 → 22.9, gate missed;
reverted.

## Per-CPU heaps (declined)

Phase 0 gate: proceed only if runic loses > 2× to the best competitor on
`spawn_churn` Cost or peak RSS, or heap lifecycle is > 10% of `spawn_churn`
user cycles. `CPUS=0-7`, 5 s × 5; RSS unpinned on 96 CPUs:

```text
                    runic   system  mimalloc  snmalloc  jemalloc
spawn_churn          379      482      668       604      1126
oversubscribed      27.5     55.1     40.6      38.2      50.9
peak RSS MiB
  spawn_churn       14.9     10.3     14.1      12.1      55.6
  oversubscribed    19.7     15.5     19.6      17.5    1037.1
minflt spawn_churn   726      130       91        89       734
```

runic wins Cost on both; RSS is ≤ 1.44× system. Lifecycle is 16–21% of user
cycles, but user cycles are ~6% of wall, so removing all of it is ≤ 7% wall.
Not worth a rewrite of ownership.

A `spawn_churn` fault campaign (one-page `extend`, lazy `Arena` directories,
direct in-space run indexing, extent-only PageMap, front-header) dropped faults
726 → 211–218 and churn Cost 307.0 → 302.4, but isolated application workloads
were flat or slightly worse. All churn-only experiments were reverted.

## Remote free

`channel_pipeline` exposed a claim/enqueue lifecycle race: an owner could accept
a claim from an already-queued run, reclaim the heap, and advance its generation
before that freer retried enqueue after close. Claimed enqueue now retries
Active/Draining transitions and treats generation advance as proof of
acceptance. Remaining allocator cost on that path is the required fail-closed
remote claim (`lock bts`), not directory lookup.

Fan-in leaf split: `free_fail` second `PageMap::get` vs `free_remote` `lock bts`.
Kept `ThreadFreeError::Remote(PageOwner)` (drop the second get).

Draining remote-free Cost after lock-free `Heaps::get` + reclaim gate (vs the
same dirty tree, not vs snmalloc):

```text
workload            before    after    vs before
scoped_map_reduce     613      387     0.63×
arc_share_drop        412      321     0.78×
channel_pipeline      912      680     0.75×
large_buffers       77706    77531     unchanged
```

`Heap::reclaim` scans only when a Draining free emptied its owner.

## Header, LTO, and policy

`Run::header_of` reads the first `repr(C)` word as raw `usize` and validates
`base` before constructing a `Run` pointer.

`[profile.bench] lto = "fat"`, `codegen-units = 1`: `vec_many_small` runic
44.8 → 32.6 (−27%); snmalloc 39.4 → 33.2 (−16%). Forced frame pointers were
dropped so Cost matches `cargo bench`.

Track C Cost after adopt + in-page header + then-current lazy-zero ≥256 KiB
(`RUNIC_PROFILE_CPUS=0-3` for threaded):

```text
workload            runic     snmalloc   mimalloc    vs sn
vec_many_small      15.8        33.4      34.5     0.47×
json_api            4608        4762      4774     0.97×
channel_pipeline    1071        1469      1965     0.73×
arc_share_drop       260         313       402     0.83×
scoped_map_reduce    595         544       670     1.09×
large_buffers       3576        4921     67425     0.73×
```

Keep vs Discard vs Unmap on `large_buffers` (64 ops, `perf stat -r 5`):

```text
policy              cycles     minflt   rss_after
Keep (default)     7.60M       2186     21.5 MiB
Discard            1.64M        141     12.5 MiB
Unmap              1.69M        132     12.6 MiB
snmalloc           1.71M        121     12.4 MiB
mimalloc           6.28M          6     24.6 MiB
```

Discard matches snmalloc on zeroed reuse. Keep pays dirty memset. Default stays
Keep. `runic:discard/keep` is the opt-in. Mixed-size 64 KiB–1 MiB under Keep now
discards on Zeroed reuse ≥64 KiB.

Policy sweep (N=5, train/hold-out): no candidate beat Keep/Keep by ≥5% geomean
without a hold-out or train regression. Run Discard is 2–8× on small churn.

RSS / minflt (syscall tracepoints were not permitted on this host):

```text
workload            runic peak/after/flt     snmalloc
vec_many_small      12.6 / 12.6 / 23         12.2 / 12.2 / 0
http_buffers        12.6 / 12.6 / 5          12.3 / 12.3 / 1
channel_pipeline    13.4 / 13.4 / 163        12.5 / 12.5 / 29
arc_share_drop      13.2 / 13.2 / 123        12.5 / 12.5 / 25
scoped_map_reduce   13.0 / 13.0 / 90         12.4 / 12.4 / 26
large_buffers       21.5 / 21.5 / 2188       12.7 / 12.4 / 121
```

Runic Keep holds large mappings (`rss_after_free == peak`). That is policy, not
a hack.

## Beyond-parity campaign

Runic-only Cost, 5s×5, LTO, this host. Gates vs runic p0, not competitors.

```text
p0 cycles/elem:
vec_push_clear            3.352
vec_many_small           31.792
string_building          37.953
hashmap_insert_remove    71.529
arc_clone_drop           42.992
mixed_collections        88.336
tree                    383.668
word_count              268.698
json_api               4571.556
regex_search           1143.047
http_buffers            169.719
large_buffers          4888.862
large_buffers_dirty   77034.772
run_churn_bursty         44.639
channel_pipeline       1279.049
arc_share_drop          270.810
scoped_map_reduce       560.804
```

Landed: hit `Run::free` without discard / `push_available`; `issued` / `link` /
claims on `RemoteLine`; live counts on `RunHeap` / `ExtentHeap`. Reverted: N=4
adopted slots (+18% `channel_pipeline`). Not tried: freelist prefetch (next-link
load 0.43%).

## Application-workload saturation

Criterion midpoint median in µs, four order-balanced self-comparison rounds
(includes workloads later dropped from the suite):

```text
vec_many_small             133.835
string_building            305.315
hashmap_insert_remove      304.400
mixed_collections          183.910
tree                       402.165
word_count                1131.800
vec_growth_log            1578.050
hashmap_grow             10210.000
vecdeque_events            864.075
text_index               45313.000
lru_cache                 8394.200
records_sort              4628.800
graph_shortest_path        462.770
json_api                  1195.100
regex_search              2678.550
http_buffers               160.575
http_parse                2979.850
csv_pipeline              1719.700
compress_roundtrip       12255.000
toml_config               5456.950
channel_pipeline          1442.550
arc_share_drop             785.530
scoped_map_reduce          497.590
async_server              1181.300
thread_pool_jobs           526.005
log_pipeline             12454.000
```

Self-gate noise: geomean −0.228%; per-workload range −1.256% to +0.652%.

Reclaim scan elimination: reverted; +0.478% geomean, worst `lru_cache` +14.291%.
Realloc known-owner reuse: confirming 8-round screen +0.027% with `hashmap_grow`
+2.187%; reverted.

Second pass on remaining allocator share: `text_index` alloc 3.73% / realloc
3.20% (tree work 88.62%); `hashmap_grow` realloc 9.66%; `log_pipeline` realloc
6.18% / remote free 2.15%. No cache, lifecycle, or policy experiment remained
above its profile threshold on the application corpus.

## Tier 2 vs snmalloc (after leak fix)

```text
phase                         runic      sn      ×sn
sh6bench/1                    5776    10066     0.57
size_boundary_sweep           47.9     49.1     0.97
small_biased_random            262      372     0.70
remote_fan_in/4/live:256      7619     4055     1.88
free_ring/4/live:256          2081     1613     1.29
```

`profile.sh` is user-cycles only. Pair it with `page-faults` + `time` sys on
mixed/large benches.

## Architecture campaign (`perf/architecture-campaign`)

Branch from `origin/master` `1128acc`. Corpus gained `shard_aggregator`,
`buffer_pool`, and `arc_broadcast`. Isolated experiments vs Criterion
`--save-baseline pre-tf` (`taskset -c 0-3`, sample_size 10). Gate: geomean up
and no workload >1% slower. All architecture diffs reverted; workloads kept.

```text
deferred remote list   lose: first cut SIGSEGV (bitmap drain reused a block
                       still being linked); after steal-only accept,
                       thread_pool_jobs ~−15% thrpt, async_server/log_pipeline
                       also slower
BatchIt last-run skip  no causal remote win; single-thread “gains” matched
                       cold first baseline; arc_broadcast estimate >1% slower
fullness bins          lose: regex_search ~−8%, vec_growth_log ~−5%, lru_cache
                       ~−3%
empty-run restamp      initially blocked by in-map headers
2 MiB maps + decay     lose: unbind DONTNEED; thread_pool_jobs ~−56% thrpt,
                       shard_aggregator ~−75%, buffer_pool ~−68%
```

Follow-up completed the remaining variants. `profile.sh` first measured
`free_remote` at 11.42% and `Heap::flush` at 3.75% on `thread_pool_jobs`;
`shard_aggregator` was mostly Draining (`Heaps::free` 15.52%), not inbox CAS.
Baseline peak RSS (KiB): thread pool 12664, log 18040, shard 12688, buffer
12664, Arc 12572.

```text
bounded BatchIt       four Run ways, eight claims/way, flush on eviction/TLS
                      exit; unit + remote stress passed, async_server aborted
                      in optimized Criterion; reverted. HeapId outbox stopped:
                      inbox CAS was not the sampled wall.
empty-run pool        safely removed empty current runs from donor directories,
                      retained donor map ownership, re-ID/restamped on acquire;
                      lost broadly (lru_cache −14.1%, regex −10.3%,
                      vecdeque −7.1%); reverted. This is also the run
                      abandon/reclaim mechanism, so no larger pool retry.
2 MiB map only        retained untrimmed over-map and aligned run maps to 2 MiB;
                      shard_aggregator −7.5%, buffer_pool −2.0%; reverted.
two-generation decay isolated DONTNEED after two empty unbind scans;
                      regex −6.2%, log −5.0%, shard −6.4%, buffer −6.8%;
                      reverted.
extent budget         64/64 MiB → 128/128 MiB; no causal large-workload win,
                      shard/async/Arc midpoint estimates worse than 1%;
                      reverted. Cache keys are already page-rounded mapping
                      lengths, so page-rounded exact reuse was already present.
```

The out-of-scope closeout then tried the remaining families against baseline
`out-of-campaign` on the same pinned Criterion corpus. This host has two NUMA
nodes and THP `always`; it has no reserved hugetlb pages. Time deltas below are
Criterion midpoints (negative is faster). Every allocator diff was reverted.

```text
first-fit extents     want ≤ have ≤ 2*want: geomean −0.30%, but csv +3.19%,
                      thread_pool +2.34%, json +1.71%; rejected.
THP-aligned maps      2 MiB-align run maps and extents ≥2 MiB: geomean −1.09%,
                      but thread_pool +3.80%, json +2.65%; rejected. Explicit
                      MAP_HUGETLB cannot run here (HugePages_Total=0).
NUMA local mbind      bind fresh maps MPOL_PREFERRED to the allocating thread's
                      node: geomean +1.49%, regex +10.66%, lru +7.29%; rejected.
TLS magazine         eight blocks per class ahead of the current run: geomean
                      +1.49%, vec_growth +12.16%, regex +11.11%; rejected.
one-page extend      remove the 32-block minimum: geomean −0.54%, but
                      thread_pool +1.36%; rejected.
rseq-rs front        rseq-rs 0.8.0 at f208f9b, one still-live pointer per CPU
                      and class: first 15 workloads geomean +27.24% (vecdeque
                      +94.32%), then thread_pool aborted during TLS teardown;
                      rejected for both throughput and correctness.
```

## Unsafe-leaf architecture cleanup

Replaced higher-layer raw owner handles with process-lifetime `&Heap` and
process-lifetime `PageOwner` entities; moved run state to `Cell`, extent cache links
to `ExtentId`, and inbox queue/link traversal behind `Inbox<T>`. Run/extent
identity callers derive `HeapId` from the owning heap. Live edges update
atomics on `Heap`; reclaim confirms them by scanning the run/extent arenas.
Extent slots remain immortal when their mappings are dropped.

Gate on this host: workspace tests, strict Clippy, and bench build pass.
`vec_growth_log` profile: 360.388 cycles/element, 10.586 Melem/s; Criterion
midpoint throughput changed +0.31%. The first unpinned `arc_broadcast` sample
was 8.91 ms; a dedicated 20-sample, 10-second rerun was 4.20 ms.

The final stack audit found and fixed a lifecycle race inherited from the old
shape: reclaim loaded Draining twice and then unconditionally stored Free, so
it could overwrite a concurrent Draining→Active adoption. Adoption now locks
`HeapInner` before its CAS and keeps the guard for the first flush; reclaim
bumps the generation with a CAS. TLS retains the generation captured at
bind/adopt because it is an independent stale-incarnation token, not a second
owner handle. Flush errors now remain typed until the sole allocator abort
sink instead of becoming null allocation results.

Paired `profile.sh` runs (three 2-second perf-stat repeats, pinned CPU) compared
`origin/master` with the repaired tree on remote-heavy real workloads. Cycles
per element changed: `thread_pool_jobs` 3771.675→3758.070 (-0.36%),
`shard_aggregator` 460.468→464.074 (+0.78%), `buffer_pool`
9638.356→9421.610 (-2.25%), and `arc_broadcast` 2275.956→2241.304
(-1.52%). No workload regressed beyond the 1% gate.

The final unsafe-leaf pass kept the lock-free owner-free protocol, stored typed
run references in TLS and the run directory, and moved dirty extent zeroing onto
`Extent`. A second paired screen against `origin/master` (three 2-second
perf-stat repeats, CPUs 24–27) measured cycles/element:
`thread_pool_jobs` 6152.260→6035.207 (-1.90%),
`shard_aggregator` 535.509→489.530 (-8.59%),
`buffer_pool` 17512.680→17150.313 (-2.07%), and
`arc_broadcast` 9907.942→9933.413 (+0.26%). No workload crossed the 1% regression
gate.

Read that screen as "did not regress", not as a win. The pass is ownership and
type cleanup with no protocol change, so it should be performance-neutral. The
`shard_aggregator` delta is noise: total cycles rose (8.24G→8.30G) and only the
Criterion element rate moved (6.53→7.28 Melem/s), and an earlier screen of the
same tree reported +0.78% on that workload. Two-second threaded samples on this
host carry swings of that size.

State cleanup then replaced `available_next` + `on_available` with
`AvailableLink::{Unlisted,Tail,Next}` and folded the separate retired bit into
`HeapMode::Retired`. True binary synchronization/observations stayed bools.
The same pinned gate against `origin/master` measured cycles/element:
`thread_pool_jobs` 6152.260→6116.179 (-0.59%),
`shard_aggregator` 535.509→474.701 (-11.36%, threaded noise),
`buffer_pool` 17512.680→17468.074 (-0.25%), and
`arc_broadcast` 9907.942→9608.833 (-3.02%). No regression crossed 1%; as above,
these are gate results, not claimed improvements.

TLS then dropped the bound/adopted field pair. Process `Heap`/`Heaps` stay;
one thread-owned heap is `ThreadHeap::{Vacant, Active}` and the frontend is
`THREAD_HEAPS`. Hit paths still do not load TLS heap slots. Paired 2s×3
`profile.sh` vs this branch HEAD (CPUs 24–27) kept instruction counts flat on
the hit corpus (word_count / hashmap_grow / graph_shortest_path within 0.2%).
Cycles/elem moved more than 1% on some 2s samples (`hashmap_grow` +4.5% cpe with
−0.2% instructions); treat as the same host noise as earlier screens, not a
protocol change.

## Post-#165 deep screen (`21b20ed`, CPUs 24–27)

Pinned Criterion, 20 samples × 2 s, all five `global_*` allocators. Geomean of
runic / best competitor = **1.050×**. Runic is best on 6/20 workloads
(`arc_broadcast`, `buffer_pool`, `json_api`, `records_sort`, `vec_growth_log`,
`vecdeque_events`). Hit-ish `word_count` is 1.00× snmalloc.

Criterion mean time vs best (only ≥5% gaps):

```text
                    runic     best     vs
log_pipeline       13.308   10.888 sn  1.22×
thread_pool_jobs    1.026    0.876 sn  1.17×
lru_cache          10.170    8.694 mi  1.17×
shard_aggregator    0.518    0.456 sn  1.13×
http_parse          2.934    2.655 sn  1.11×
hashmap_grow       10.555   10.095 sn  1.05×
text_index         50.379   48.011 sn  1.05×
```

Paired `profile.sh` (3×2 s stat + 5 s `cycles:u` record), cycles/elem runic vs
competitor (after = competitor, so ratio < 1 means competitor cheaper):

```text
                   runic cpe    other cpe    insn/elem
word_count           263.7     261.4 sn     741.6 vs 706.5
log_pipeline       16796.4   14168.1 sn   21555 vs 19740
thread_pool_jobs    5896.7    5443.3 sn   10046 vs 9182
lru_cache           1141.0     945.7 mi    2787 vs 2783
shard_aggregator     536.9     483.5 sn    1403 vs 1118
http_parse          2628.5    2339.1 sn    7152 vs 6760
```

Where (flat `cycles:u`):

- `word_count`: workload 48%, then fmt/string; `__rust_realloc` 6.9%,
  `__rust_alloc` 2.7%. No `ThreadHeaps` / `free_remote` in the top 15. Hit is
  already off the profile.
- `thread_pool_jobs`: `Allocator::free_remote` 10.7%, `Heap::flush` 3.7%,
  `free_slow` 2.1%, `free_owner` 1.6%, `dealloc_slow` 1.6%. snmalloc's
  `dealloc_remote` is 3.3% on the same workload. Remaining allocator time is
  remote free, not the owner hit.
- `shard_aggregator`: `Heaps::admit` 12.9%, `free_remote` 6.8%, `free_owner`
  5.4%, `Heaps::free` 5.0%, `dealloc_slow` 4.8%, `HeapInner::free` 4.3%,
  `ThreadHeaps::adopt` 3.1%. Third-heap adopt staying on `Heaps::free` is
  visible here.
- `log_pipeline`: regex 27%; `__rust_realloc` 7.3%, `__rust_alloc` 4.3%,
  `free_remote` 2.8%. snmalloc's top allocator symbol is `sn_rust_alloc` 1.7%.
- `lru_cache` vs mimalloc: instruction counts match (−0.13%); mimalloc IPC
  2.94 vs runic 2.44. Not an instruction-diet miss. `__rust_realloc` is 8.8%
  on runic; mimalloc spreads realloc across `mi_free` / `_mi_theap_realloc_zero`.

No further hit-path fold is supported by this profile. The remaining cost is
remote admission/`Heaps::admit`/adopt on the threaded corpus, and realloc of growing
`String`s. Those sit on already-declined or scoped-out levers (`multi-slot
adopt`, `realloc known-owner reuse`, `BatchIt-on-Inbox`). No A/B this pass.

## Diet `free_remote` / `Heaps::admit` (vs `21b20ed`)

Hypothesis: extra `HeapState` loads, an arena `get` on a heap already in
`owner.heap()`, and two Inner locks for claimed draining. Protocol unchanged
(no claim before adopt / `Heaps::free`; no third TLS slot).

Change: `Heap::active_id` (one load for Active routing); `Heap::admit_draining`
on `owner.heap()` for `Heaps::free` / `Heap::flush_claimed`; `Heaps::flush`
still `get`s (unbind has no owner); claimed draining queues+accepts+reclaims
under one Inner lock. `Heaps::enqueue` deleted. `Heap::enqueue` admits only
via `acquire_lease`.

Pinned `profile.sh` CPUs 24–27, 3×2 s stat. Repeat Cost used the rebuilt ELF.

```text
                    before     after     insn/elem
thread_pool_jobs    5896.7    5902.8    10046 → 10009   (~flat)
shard_aggregator     536.9    506.8     1403 → 1289    (repeat; −5.6% cpe / −8.1% insn)
word_count           263.7    267.7      742 → 735     (+1.5% cpe, −0.9% insn)
```

A second shard window printed 453.5 cpe / 1159 insn (−15%/−17%); treat that as
the same threaded noise as earlier screens. The repeat (506.8 / 1289) is the
number to keep. `Heaps::admit` is gone from Where (inlined into
`admit_draining` / `free`). dwarf `cycles:u` on shard: `free_remote` 18.9%,
`free_owner` 5.6%, `HeapInner::free` 5.0%, `dealloc_slow` 4.4%, `adopt` 3.4%.
LBR record still SIGABRTs that workload.

Active remote (`thread_pool_jobs`) did not move: `free_remote` 10.7% → 9.5%.
Hit (`word_count`) still has no `ThreadHeaps` / `free_remote` in the top 15;
the cpe bump is IPC, not extra instructions.

The draining admission change is measurable. This profile does not justify
more hit-path changes.

## Checked owner double-free (declined)

Tried out-of-band per-block `Allocated` / `Reusable` / `Claimed` state so
owner-local small double-free would abort, matching the remote `claim` and
extent paths. Immediate detection needs a hit-path load/store on every
allocate and free.

Pinned `profile.sh` CPU 0, `global/runic/hashmap_grow`, 3×3 s stat:

```text
                         cycles/elem   instructions/elem
unchecked owner free        1171.983            3689.651
locked bitmap               1498.913            4184.283
compact tri-state bytes     1226.616            3783.236
```

The cheapest fail-closed map was still +4.7% cycles per element and +2.5%
instructions per element. Leave owner double-free undefined for runs and
extents, as snmalloc and mimalloc do on the default owner hit. Remote `claim`
still detects duplicate frees. Revisit only as optional hardening.

After restoring store-not-CAS owner extent free and the original `Run::free`,
the same pin compared to `owner-df-before` is noise:

```text
                         cycles/elem   insn/elem   elem/s
owner-df-before             1171.983     3689.651  3.2471e6
owner-df-removed            1174.871     3681.067  3.2315e6
delta                          +0.25%      -0.23%    -0.48%
```

Criterion on `hashmap_grow` reported no change (p > 0.05) across the three
3s samples. Cache-misses/elem doubled in the ratio table (0.0007 to 0.0014)
on a near-zero count; ignore.

## 0.9 Fast placement screen

Payload `Hints` knobs only (run + extent maps). Metadata maps stay anonymous
4 KiB. Criterion `global_runic`, suite defaults (10 samples, 1 s), ns/iter.
This host has two NUMA nodes and THP `always`; `HugePages_Total=0`.

```text
workload                 Off       Thp     Local      Both
word_count           1164422   1167215   1161976   1163173
vec_growth_log       1647241   1639752   1629748   1648350
hashmap_grow        10068877  10037096  10129765  10457669
vecdeque_events       823269    824844    815696    840138
text_index          48150455  48210456  47664937  50008542
lru_cache            9656773   9582597   9610668   9696549
records_sort         4551331   4536499   4530817   4623067
graph_shortest_path   449534    447336    446511    451966
json_api             1205472   1213705   1220768   1244191
regex_search         2568405   2557656   2580940   2572531
http_parse           2654727   2678254   2671017   2712777
csv_pipeline         1658883   1660820   1665192   1680694
compress_roundtrip  12303137  12273311  12389654  12264971
toml_config          5431844   5423002   5449957   5489331
async_server         1212434   1915124   1201785   1215093
thread_pool_jobs     1005970   2106344   1019692   1005858
log_pipeline        12539889  13443353  12324351  12498771
shard_aggregator      432376    627031    430230    429618
buffer_pool          1089557   1841532   1816710   1085823
arc_broadcast        4139237   9029394   8922181   4211523
```

Thp and Local each more than double important threaded workloads vs Off
(`thread_pool_jobs`, `arc_broadcast`, `buffer_pool`). Both recovers those
losses but gives back 3.9% on `hashmap_grow`, 3.2% on `json_api`, 3.9% on
`text_index`, and 1.7% on `arc_broadcast`, without a material win on the
priority threaded set. Off/Off is the strongest balanced default. This is a
weighted production-workload decision, not a requirement to win every row.
Force was not a default candidate (no reserved huge pages). `MAP_HUGETLB` was
then dropped from the knobs: reserved hugetlb is a kernel deployment choice,
not a heap policy. Thp remains the hint.

A follow-up priority screen used `profile.sh` on CPUs 24–27 with three perf
stat repeats and 2-second windows. Cycles per element:

```text
workload                 Off       Thp     Local      Both
thread_pool_jobs       6043.8    5976.0    5974.5    6081.7
log_pipeline          16810.6   15135.8   16898.6   16532.3
shard_aggregator        513.3     535.5     539.3     494.8
buffer_pool           17529.5   17257.6   17189.0   17349.8
arc_broadcast          9580.6    9500.6    9485.8    9630.7
```

Thp's equal-weight geomean was 1.94% below Off, led by `log_pipeline`, but it
regressed `shard_aggregator` by 4.33%. Local regressed the geomean by 0.27%;
Both improved it by 1.04% while regressing `thread_pool_jobs`. Instruction
counts moved with the Criterion element-rate denominator, including the large
`log_pipeline` and `shard_aggregator` swings, so this short screen does not
establish a placement-caused win. It does reject Local as a default and does
not overturn Off/Off: Off remains the balanced, explicit baseline while Thp
and Local stay opt-in.

## Inbox list split and policy folds

`List` split out of `Inbox`, `RunHeap::release` on the slow free, lookup and
the realloc copy fallback on `Allocator`, and both miss paths calling
`Heap::flush`. Criterion `global_runic` on CPUs 24-27, suite defaults, branch
against a saved `master` (0.9.0) baseline:

```text
workload              change    p
word_count            +1.57%    0.03
vec_growth_log        -2.02%    0.00
hashmap_grow          -0.51%    0.12
vecdeque_events       -0.56%    0.04
text_index            +0.92%    0.00
graph_shortest_path   -1.77%    0.00
json_api              +3.21%    0.00
http_parse            -0.82%    0.00
thread_pool_jobs      -3.16%    0.00
others                within noise (p > 0.05)
```

Paired 5 s, 20 sample reruns: `json_api` -1.16%, `word_count` +1.74%. The
1 s `json_api` swing was noise. `word_count` under `profile.sh` on CPU 24
(three 2 s repeats): cycles per element 271.41 to 271.20 (-0.08%),
instructions per element 734.98 to 735.88 (+0.12%), elements per second
+0.26%. No regression in the counters; the Criterion delta is run-to-run
timing noise. Retained.


## 0.10 Safe split and the run `Freelist`

Extent owner free became a CAS `Allocated -> Free` in both builds (glibc
checks large chunks in its default build; mimalloc and snmalloc do not check
small owner double-free by default, so Fast leaves that undefined). The Safe
small check follows mimalloc `MI_SECURE=4`: read the block's first word, and
walk the freelist only when it is `0` or a block address of this run. The
run free-block stack moved into `heap/run/freelist.rs` as `Freelist`.

Criterion `global_runic`, CPUs 0-3, `buffer_pool` and `hashmap_grow` against
a saved baseline taken before the CAS and the `Freelist` move:

```text
                          hashmap_grow   buffer_pool
CAS + Freelist                 +5.73%        +1.24%
CAS reverted, Freelist         +6.34%        +0.27%
CAS, Freelist reverted         +0.49%        -0.11%
```

The CAS is not the cost. `objdump` of `__rust_alloc` and `__rust_dealloc` is
identical between the `Freelist` and inline variants apart from relocations.
`Run::extend` differed: the first `push_contiguous` multiplied
`step * stride` per block and LLVM unrolled it 8x with six callee-saved
pushes and stack spills. Rewriting it as a pointer walk (`byte_add(stride)`)
gave a tight 8x loop and cut `hashmap_grow` to +3.27%.

Pinned CPU 2, `perf stat`, `hashmap_grow`: instructions 17.113e9 (inline)
vs 17.118e9 (`Freelist`), +0.03%; cycles +0.94%. Paired A/B on the same
core, two rounds: +0.57% (p 0.49), +1.57% (p 0.07). Full suite A/B on CPU 2
moved both ways: `lru_cache` +16.7%, `vec_growth_log` +5.6%,
`regex_search` -6.5%, `arc_broadcast` -4.5%, `records_sort` -3.8%,
`vecdeque_events` -3.6%. `perf record` on `lru_cache`: 66% (inline) vs 70%
(`Freelist`) of cycles are the workload's own `recency.iter().position` scan;
`__rust_realloc` is 9.7% vs 8.8%. The allocator code is the same, so the
swings are code placement of the benchmark under fat LTO with one codegen
unit; the extra module shifts every later function by 0x30 bytes. Retained.
Placement swings of this size mean single-workload Criterion deltas below
the `perf stat` instruction check are not evidence on their own.

## List, queue, and N adopted heaps

Owner-exclusive linkage is `LinkedList` and `Queue` (`Cell` links). Lock-free
chains are `Mpsc` (inbox `drain`) and `Mpmc` (free-heap stack), both on one
Treiber stack. Available runs moved onto `Queue`. The extent cache and
unmapped slots share one `LinkedList`. TLS heaps are that list too: bind
`push_front`, adopt `push_back`. The last heap stays attached.

Pinned `perf stat -r 3`, `instructions:u` / `cycles:u`, Criterion 1 s x 10
samples. `hashmap_grow` on CPU 0, `shard_aggregator` on CPUs 0-3. Baseline is
`daee6e9`.

```text
                    instructions        cycles
hashmap_grow master   17,117,893,470   5,528,103,242
hashmap_grow list     17,219,219,744   5,559,447,517   (+0.59% insn, +0.57% cycles)
shard master          13,233,470,567   5,054,808,738
shard list            12,348,385,474   4,784,770,294   (-6.7% insn, -5.3% cycles)
```

`hashmap_grow::run` differs from master only by nop padding (1914 vs 1913
instructions). The current-run hit did not grow. The process-wide instruction
increase is the slow owner free walking the heap list. Criterion time on
`hashmap_grow` stayed inside the master's own repeat spread (~10.7 ms).
`shard_aggregator` is the threaded remote-free workload and got faster.
A fixed four-slot TLS array previously lost about 18% on `channel_pipeline`.
This list does not. Retained.

## Shorter locks on adopted heaps

Active `flush_owner` accepts each inbox node with no `HeapInner`, then takes a
guard only to `push_available` or `cache_or_unmap` that node. `adopt` holds the
guard through its own flush: dropping it and publishing with `require_inner`
let a losing adopter abort the new owner. `alloc_miss` and `alloc_extent` flush
one heap at a time and take that heap's guard only to reuse or map. Draining `Heap::flush` walks the same nodes under the admit guard
so accept, publish, and reclaim stay one section. The locked `RunHeap::accept`
and `ExtentHeap::accept` are gone. `idle` no longer `try_lock`s or scans
`has_live`.

Same pin as above, same `daee6e9` baseline, after the list change.

```text
                    instructions        cycles
hashmap_grow master   17,117,934,851   5,501,844,899
hashmap_grow locks    17,219,028,412   5,591,458,222   (+0.59% insn, +1.6% cycles)
shard master          13,220,823,032   5,074,932,177
shard locks           12,536,156,566   4,832,695,591   (-5.2% insn, -4.8% cycles)
```

`hashmap_grow` instructions match the list measurement (17,219,219,744 vs
17,219,028,412). This lock is not on the hit. `shard_aggregator` stayed in
the list's band (12,348,385,474 instructions). One shard sample with a 17%
spread was discarded; the repeat above is ±0.75%. Retained.

### Reuse from every attached heap

Alloc miss then flushes each adopted heap and takes a run it already holds
(`take_available`), then the front heap `acquire`s (own list, then a new run).
Large allocation tries `reuse_cached` on each adopted heap, then the front heap
`allocate`s (own cache, then a fresh mapping). Adopted heaps come first so
they never map and drain toward idle. One guard per heap, dropped before the
next. Before this the miss only ever looked at the front heap, so adopted
heaps' runs and cached extents waited for their freer.

```text
                      instructions        cycles
hashmap_grow reuse    17,219,188,627   5,560,091,921   (flat vs locks)
shard reuse           12,447,273,275   4,793,154,670   (-0.7% insn, -0.8% cycles vs locks)
```

`hashmap_grow` is flat (one heap, an empty adopted walk). `shard_aggregator`
is inside its band (12.3 to 12.5B). Retained.

### Every attached heap allocates

The front heap was the only one allowed to map, so an adopted heap could only
hand over a run it already held and then go idle. That is a second kind of
heap. Miss now flushes every attached heap and takes a run it already holds,
then calls `acquire` on each until one maps. Large allocation does the same
with `reuse_cached`, then `allocate`. List order is unchanged: bind still
links at the front, adopt at the back, and `idle` still keeps the last heap.
The head maps when it can because it is first, not because it has a different
rule. A heap later in the list maps when the ones before it cannot.

```text
                      instructions        cycles
hashmap_grow peers    17,219,166,731   5,590,526,764   (flat vs reuse)
shard peers           12,255,393,847   4,737,322,529   (inside the 12.3 to 12.5B band)
```

One shard sample with a 17% spread was discarded. The repeat above is ±0.76%.
Retained.

### Pre-merge screen against `daee6e9`

Pinned `perf stat`, `instructions:u` / `cycles:u`, Criterion 1 s x 10 samples,
`-r 3`. Single-thread workloads on CPU 0. Threaded workloads (`async_server`,
`thread_pool_jobs`, `log_pipeline`, `shard_aggregator`, `buffer_pool`,
`arc_broadcast`) on CPUs 0-3. The new ELF includes the uncommitted peer-heap
walk. Samples with a spread above 3% were rerun.

Stable against master: `hashmap_grow` 17,219,199,439 instructions (+0.59%),
the same count as the list and peer measurements. `word_count`, `text_index`,
`graph_shortest_path`, `json_api`, `csv_pipeline`, `compress_roundtrip`,
`toml_config`, `buffer_pool`, and `thread_pool_jobs` stay inside 1% of
instructions. `records_sort`, `regex_search`, and `http_parse` looked large
on the first pass and came back to 0% on the rerun.

`shard_aggregator` rerun (spread 1.12%): 12,163,672,267 instructions and
4,773,844,170 cycles, -6.4% instructions and -4.3% cycles versus master
12,991,119,084 / 4,989,247,572. A first sample at -34% had a 43% spread and
was discarded. `log_pipeline` is -1.7% instructions. `vecdeque_events` is
-3.1% instructions with flat cycles.

Cycle profiles, 8 s, `cycles:u`. On master, `shard_aggregator` spends 11.7%
in `Heaps::admit`, 6.9% in `free_remote`, and 3.4% in `adopt`. On the new
binary those drop out of the top; `ThreadHeaps::free_owner` is 14% and the
workload rises from 40% to 53%. The miss path does not show up.
`free_owner` is the allocator time that remains.

`lru_cache` is repeatable at +39% instructions and +20% cycles (spread 0.2%).
Instruction samples put 71% in `lru_cache::run` and 9.4% in `__rust_realloc`
on both binaries. The function is 1613 instructions on master and 1608 on
the new ELF. Same shape as the earlier fat-LTO placement swing on this
workload. `async_server` (`-r 5`) is +1.3% instructions and +3.5% cycles.
`free_remote` stays near 3% and `__rust_alloc` near 2.4%; tokio and httparse
are the rest.

`__rust_alloc` is 82 instructions on master and 95 on the new ELF.
`__rust_dealloc` is 71 and 64. `__rust_realloc` is 418 on both.
`ThreadHeaps::alloc_miss` is 134 and 162. Retained.

### Cross-allocator screen (working tree, CPUs 0 and 0-3)

Pinned Criterion median time, 10 samples x 1 s, warmup 250 ms. Single-thread
workloads on CPU 0. Threaded workloads on CPUs 0-3. Runic is the peer-heap
ELF from the pre-merge screen. snmalloc, mimalloc, jemalloc, and glibc were
built in the same pass. Every confidence-interval spread stayed under 5%.

Geomean of runic / best competitor = **1.029×**. Runic is fastest on 7/20
(`buffer_pool`, `json_api`, `records_sort`, `text_index`, `toml_config`,
`vec_growth_log`, `vecdeque_events`). Geomean versus each allocator: snmalloc
1.025×, mimalloc 0.961×, jemalloc 0.911×, system 0.768×.

Median time in ms, and runic / best:

```text
                      runic       sn       mi       je      sys   vs
word_count            1.159    1.125    1.217    1.549    1.408  1.03 sn
vec_growth_log        1.512    1.660    1.619    1.709    2.362  0.93 mi
hashmap_grow         10.326    9.716   10.494   12.793   13.478  1.06 sn
vecdeque_events       0.852    0.880    0.966    0.928    1.395  0.97 sn
text_index           46.887   48.910   48.120   49.240   55.042  0.97 mi
lru_cache             8.251    7.953    9.708    9.602   12.714  1.04 sn
records_sort          4.598    4.745    5.013    5.804    7.543  0.97 sn
graph_shortest_path   0.462    0.441    0.431    0.473    0.535  1.07 mi
json_api              1.230    1.267    1.295    1.492    2.103  0.97 sn
regex_search          2.552    2.446    2.629    2.470    2.558  1.04 sn
http_parse            2.653    2.570    2.733    2.731    3.273  1.03 sn
csv_pipeline          1.712    1.574    1.758    1.690    2.017  1.09 sn
compress_roundtrip   12.350   12.247   12.309   12.712   14.732  1.01 sn
toml_config           5.396    5.463    5.716    6.172    7.673  0.99 sn
async_server          1.705    1.667    1.734    1.788    2.593  1.02 sn
thread_pool_jobs      1.053    0.873    0.949    0.960    1.104  1.21 sn
log_pipeline         12.858   10.934   13.055   12.664   15.146  1.18 sn
shard_aggregator      0.470    0.446    0.537    0.642    0.552  1.05 sn
buffer_pool           1.243    1.295    1.287    1.400    1.723  0.97 mi
arc_broadcast         5.978    5.897    6.259    5.907    6.421  1.01 sn
```

The `21b20ed` screen (1.050×, CPUs 24-27, 20 samples x 2 s) is a different
protocol, so the geomeans are not a paired delta. On this protocol the gaps
at or above 5% are `thread_pool_jobs` 1.21× snmalloc, `log_pipeline` 1.18×
snmalloc, `csv_pipeline` 1.09× snmalloc, `graph_shortest_path` 1.07×
mimalloc, `hashmap_grow` 1.06× snmalloc, and `shard_aggregator` 1.05×
snmalloc.

### Remote chains and extent state, gap set

Working tree after the thread slots (8 slots, chains of 16, front slot is the
run just freed) and Fast extent stores. Same protocol as the screen above:
CPUs 0-3, 10 samples x 1 s, warmup 250 ms. Paired with the snmalloc binary
from that screen. The extent store is not on these paths. No workload frees
a large object in the loop. `buffer_pool` retains its 64 KiB vectors.

```text
                      runic      sn     vs sn    prior runic / sn
thread_pool_jobs      1.034    0.882   1.17×    1.053 / 0.873  1.21×
log_pipeline         13.245   10.846   1.22×    12.858 / 10.934 1.18×
shard_aggregator      0.492    0.443   1.11×    0.470 / 0.446  1.05×
csv_pipeline          1.725    1.592   1.08×    1.712 / 1.574  1.09×
```

`thread_pool_jobs` interval is 1.030 to 1.037 ms. `csv_pipeline` is flat.
`shard_aggregator` is slower than the pre-batch screen (0.470 to 0.492 ms,
interval 0.490 to 0.494). `log_pipeline` was rerun. Both passes have a
spread above 5% (first 13.230 to 14.007, rerun 12.773 to 13.787), so that
ratio is not a kept delta.

## Test revamp and the unbound freer exit hook

The new `threads.rs` case `remote_frees_across_many_runs_all_return_to_the_owner`
found that a thread which only frees never binds, so its exit callback was
never armed and partial chains left on its slots were lost when it exited.
The fix arms the callback in `bring_front`, the first remote hold of a run.

`bring_front` is cold but runs on every remote-free slot miss, so it was
measured. Criterion `global_runic`, CPUs shared, 10 samples x 1 s, warmup
250 ms, each row paired against one saved baseline of the old code.

```text
                      old vs baseline   new vs baseline (three runs)
thread_pool_jobs      -2.4% (p 0.20)    -5.0%  -5.6%  -4.8%
shard_aggregator     +14.8% (p 0.00)   +10.7%  +5.3%  +3.5% (p 0.11)
arc_broadcast              -           +2.0% (p 0.52)
```

The old code against its own baseline moves 14.8% on `shard_aggregator`, so
the machine noise is larger than any of the new deltas. Not a kept delta in
either direction.
