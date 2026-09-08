# AGENTS.md

Scope: `crates/runic-bench/`.

- Internal (`publish = false`). Deterministic workloads; do not change allocator semantics to win a bench.
- Touch allocated memory so work is not optimized away.
- Benches are process-global (`#[global_allocator]`): std collections plus `serde_json` / `regex` / `bytes`. No synthetic `GlobalAlloc` ports.
- After changes: `cargo bench -p runic-bench --no-run`. Perf claims need paired `scripts/profile.sh` (prefer `--compare`; fresh bench bins) — do not gate on Criterion alone or inferred wins.
