# runic-bench/benches

Thin Criterion entry points. Registration lives in `src/suite/`.

## Targets

- `global_{runic,system,mimalloc,jemalloc,snmalloc}`: the same application
  workloads under each process-global allocator.

## Naming

- `global/<alloc>/<workload>` — `global/runic/word_count`,
  `global/runic/json_api`

## Filters

- `global/runic/word_count`
- `global/runic/vec_growth_log`
- `global/runic/hashmap_grow`
- `global/runic/vecdeque_events`
- `global/runic/text_index`
- `global/runic/lru_cache`
- `global/runic/records_sort`
- `global/runic/graph_shortest_path`
- `global/runic/json_api`
- `global/runic/regex_search`
- `global/runic/http_parse`
- `global/runic/csv_pipeline`
- `global/runic/compress_roundtrip`
- `global/runic/toml_config`
- `global/runic/async_server`
- `global/runic/thread_pool_jobs`
- `global/runic/log_pipeline`
- `global/runic/shard_aggregator`
- `global/runic/buffer_pool`
- `global/runic/arc_broadcast`

Use `RUNIC_PROFILE_CPUS=0-3` for threaded workloads.

## Run

```sh
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench global_runic -- 'global/runic/json_api' --exact
```

## Perf

`scripts/profile.sh` wraps the resolved ELF. Cost is `metrics.txt` / `--compare`.

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline global_runic 'global/runic/word_count'
scripts/profile.sh -l baseline global_runic 'global/runic/json_api'
scripts/profile.sh --with callgrind global_runic 'global/runic/http_parse'
```
