# runic-bench/src/bin

Benchmark support binaries.

`rss` runs isolated allocator/workload combinations and reports resident-set size. It uses fresh subprocesses so one allocator's cached memory does not affect another row.

`policy_grid` runs configured Runic allocator variants in fresh subprocesses and reports the same RSS/timing CSV shape for policy comparison.

## Run

```sh
cargo run -p runic-bench --bin rss
cargo run -p runic-bench --release --bin policy_grid
```
