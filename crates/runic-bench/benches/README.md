# runic-bench/benches

Thin Criterion entry points. Registration lives in `src/suite/`.

## Targets

- `micro`: direct `GlobalAlloc` for every allocator (`runic`, `system`, `mimalloc`, `jemalloc`, `snmalloc`). Filter selects one.
- `threaded`: persistent workers + `lifecycle`.
- `programs`: larson, xmalloc, cache_thrash, cache_scratch, sh6bench, cfrac.
- `global_{runic,system,mimalloc,jemalloc,snmalloc}`: `#[global_allocator]` collections.

## Naming

- `micro/<group>/<alloc>/<param>` — `micro/owner_free/runic/64`, `micro/recycled_churn/runic/64/live:256`
- `threaded/<group>/<alloc>/<threads>[/live:N]` — `threaded/remote_fan_in/runic/4/live:256`
- `threaded/lifecycle/<alloc>/<threads>` — spawn, churn, join
- `programs/<group>/<alloc>/<threads>` — `programs/sh6bench/runic/4`
- `global/<alloc>/<group>` — `global/runic/tree`

## Filters

Phase-isolated local:

- `micro/owner_free/runic/{8|64|80|4096}`
- `micro/freelist_allocate/runic/{8|64|80|4096}`

Size-class matrix:

- `micro/recycled_churn/runic/{size}/live:{depth}`
- `micro/recycled_hotspot/runic/{64|72|80|88}/live:{depth}`

Persistent threaded:

- `threaded/local_churn/runic/4`
- `threaded/free_ring/runic/4/live:256`
- `threaded/remote_fan_in/runic/4/live:256`
- `threaded/owner_concurrent/runic/4/live:256`
- `threaded/remote_reuse/runic/live:1`
- `threaded/bound_remote/runic/4`
- `threaded/unbound_remote/runic/4`
- `threaded/owner_accept/runic/4`

## Run

```sh
cargo bench -p runic-bench --bench micro
cargo bench -p runic-bench --bench programs -- 'programs/larson/runic/4'
```

## Perf

`scripts/profile.sh` wraps the resolved ELF. Cost is `metrics.txt` / `--compare`.

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline micro 'micro/single_size_churn/runic/64'
scripts/profile.sh -l baseline micro 'micro/owner_free/runic/64'
scripts/profile.sh -l baseline micro 'micro/freelist_allocate/runic/64'
scripts/profile.sh -l baseline micro 'micro/recycled_churn/runic/64/live:1'
scripts/profile.sh -l baseline -t 20 \
  threaded 'threaded/remote_fan_in/runic/4/live:256'
scripts/profile.sh -l baseline -t 20 \
  threaded 'threaded/owner_accept/runic/4'
scripts/profile.sh --with callgrind micro 'micro/owner_free/runic/64'
```
