# runic-bench/src/bin

`metrics` runs each allocator/case in a fresh subprocess and prints RSS peak, plateau after free, VMA count, minor faults, and optional syscall counts.

Runic extent configs (`runic:extent_drop`, `runic:extent_tight`) are targets here, not Criterion ids.

```sh
cargo run -p runic-bench --release --bin metrics
cargo run -p runic-bench --release --bin metrics -- --cases sh6bench,large_churn --targets runic,mimalloc
cargo run -p runic-bench --release --bin metrics -- --syscalls --threads 4 --cases larson
```
