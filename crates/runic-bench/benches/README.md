# runic-bench/benches

Thin Criterion entry points. Registration lives in `src/suite/`.

## Targets

- `global_{runic,system,mimalloc,jemalloc,snmalloc}`: `#[global_allocator]` collections and library traffic.

## Naming

- `global/<alloc>/<group>` — `global/runic/tree`, `global/runic/json_api`

## Filters

Collections:

- `global/runic/vec_push_clear`
- `global/runic/tree`
- `global/runic/word_count`

Libraries:

- `global/runic/json_api`
- `global/runic/regex_search`
- `global/runic/http_buffers`

## Run

```sh
cargo bench -p runic-bench --bench global_runic
cargo bench -p runic-bench --bench global_runic -- 'global/runic/json_api' --exact
```

## Perf

`scripts/profile.sh` wraps the resolved ELF. Cost is `metrics.txt` / `--compare`.

```sh
scripts/profile.sh --preflight
scripts/profile.sh -l baseline global_runic 'global/runic/tree'
scripts/profile.sh -l baseline global_runic 'global/runic/json_api'
scripts/profile.sh --with callgrind global_runic 'global/runic/http_buffers'
```
