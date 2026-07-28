# runic-bench/benches

Criterion benchmark entry points.

## Runic Targets

- `explicit`: Runic-only direct `GlobalAlloc` workloads.
- `threaded`: Runic-only threaded workloads (setup-lifecycle + persistent).
- `global_runic`: process-global Runic allocator workloads.

Use these targets for same-machine `perf stat` comparisons of Runic changes without noise from external allocator comparison runs.

## Manual Comparison Targets

- `compare_explicit`: direct `GlobalAlloc` workloads across Runic and external allocators.
- `compare_threaded`: threaded workloads across Runic and external allocators.
- `global_system`: process-global system allocator workloads.
- `global_mimalloc`: process-global mimalloc workloads.
- `global_jemalloc`: process-global jemalloc workloads.
- `global_snmalloc`: process-global snmalloc workloads.
- `common`: shared benchmark target setup.

## Threaded filters

Setup/lifecycle (spawn/join inside the timed region — not for hot-path profiles):

- `threaded/setup_lifecycle_thread_local_churn/runic/4`
- `threaded/setup_lifecycle_remote_free_ring/runic/4`
- `threaded/setup_lifecycle_draining_late_free/runic/4`

Persistent workers (preferred for allocator profiles):

- `threaded/persistent_local_churn/runic/4`
- `threaded/persistent_free_ring/runic/4/live:256`
- `threaded/persistent_remote_fan_in/runic/4/live:256` — freer backlog depth is real
- `threaded/persistent_owner_concurrent/runic/4/live:256` — owner-local churn + remote frees
- `threaded/persistent_remote_reuse_latency/runic/live:1` (also `live:32`, `live:256`) — Criterion time is measured reuse latency; emits `runic_mean_reuse_ns=`
- `threaded/persistent_bound_remote/runic/4` — channel-free bound freer drains
- `threaded/persistent_unbound_remote/runic/4` — channel-free unbound freer drains
- `threaded/persistent_owner_accept/runic/4` — prepare+free outside timing; owner accept/flush only

Phase-isolated local free/alloc (setup outside timed window):

- `explicit/owner_free_only/runic/{8|64|80|4096}`
- `explicit/freelist_allocate_only/runic/{8|64|80|4096}`

Local size-class matrix (full + focused hotspot subset):

- `explicit/recycled_live_churn/runic/{size}/live:{depth}` — all 27 classes
- `explicit/recycled_live_hotspot/runic/{64|72|80|88}/live:{depth}` — PoT vs non-PoT index gate

## Run

```sh
cargo bench -p runic-bench
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench compare_explicit
```

## Perf

Benches are ordinary Criterion targets. `scripts/profile.sh` only orchestrates tools on the
resolved ELF (build hygiene, CPU pin, `perf` / flamegraph / samply / callgrind).

Optimize only with four facts, in order:

1. **Cost** — `metrics.txt` (cycles/insn/branches per elem, IPC) vs self and competitors;
   `--compare` before/after. Use phase filters for free/alloc/remote.
2. **Where** — open `perf.data` with `perf report` / `perf annotate`, or samply / flamegraph.
   Inlining hides work inside symbols; annotate answers that, not name guessing.
3. **Why** — counter groups in `perf-stat.txt` (IPC, branches, cache/TLB); threaded phases for contention.
4. **Causal** — temporary A/B after Where points at a hypothesis; accept only if Cost improves
   enough (e.g. ≥5% cyc/elem) and remote gates do not regress.

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline explicit 'explicit/single_size_churn/runic/64'
scripts/profile.sh -l baseline explicit 'explicit/owner_free_only/runic/64'
scripts/profile.sh -l baseline explicit 'explicit/freelist_allocate_only/runic/64'
scripts/profile.sh -l baseline explicit 'explicit/recycled_live_churn/runic/64/live:1'
scripts/profile.sh -l baseline -t 20 \
  threaded 'threaded/persistent_remote_fan_in/runic/4/live:256'
scripts/profile.sh -l baseline -t 20 \
  threaded 'threaded/persistent_owner_accept/runic/4'
```

After a run:

```sh
OUT=target/runic-profiles/<run-dir>
perf report -i "$OUT/perf.data"
perf annotate -i "$OUT/perf.data" --symbol '<symbol from report>'
# optional:
scripts/profile.sh --with flamegraph,samply ...
samply load "$OUT/samply.json"
```

Optional Callgrind (owner-local only):

```sh
scripts/profile.sh --with callgrind explicit 'explicit/owner_free_only/runic/64'
```

Artifacts under `target/runic-profiles/` (`RUNIC_PROFILE_DIR` or `-o`):
`manifest.txt`, `metrics.txt`, `perf-stat.txt`, `perf.data`, `perf-report-*.txt`,
`summary.txt`, plus optional flamegraph/samply/callgrind.

Compare Cost:

```sh
scripts/profile.sh --compare \
  target/runic-profiles/run-before target/runic-profiles/run-after
```

Prefer separate git worktrees built from exact commits for before/after on the same machine.
