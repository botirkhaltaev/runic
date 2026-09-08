# runic-bench/src/bin

`metrics` runs each allocator/case in a fresh subprocess and prints RSS peak, plateau after free, VMA count, minor faults, and optional syscall counts.

Cases are the same collection and library workloads as `global_*`. Runic configs (`runic:extent_drop`, `runic:extent_tight`, `runic:run_discard`) are targets here, not Criterion ids.

```sh
cargo run -p runic-bench --release --bin metrics
cargo run -p runic-bench --release --bin metrics -- --cases json_api,tree --targets runic,snmalloc
cargo run -p runic-bench --release --bin metrics -- --syscalls --cases http_buffers --targets runic
```
