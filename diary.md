# Runic Measurement Diary

Session-local Cost, Where, RSS, and experiment verdicts. Not current scope —
see [`ROADMAP.md`](ROADMAP.md) for architecture and milestones.

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
freelist/64     20.4         20.4    20.0        —          19.0
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
