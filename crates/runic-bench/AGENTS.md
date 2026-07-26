# AGENTS.md

Scope: `crates/runic-bench/`.

- Internal (`publish = false`). Deterministic workloads; do not change allocator semantics to win a bench.
- Touch allocated memory so work is not optimized away.
- `cargo bench -p runic-bench --no-run` after changes.
