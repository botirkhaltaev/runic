# AGENTS.md

Scope: `crates/runic-bench/`.

- Deterministic workloads; do not change allocator semantics to win a bench.
- Touch allocated memory so work is not optimized away.
- Benches are process-global (`#[global_allocator]`) application workloads. No synthetic `GlobalAlloc` ports or lifecycle probes.
- Keep one self-contained file per workload; share only code with genuine reuse.
- After changes: build every bench, run the Criterion test pass, then profile representative workloads with `scripts/profile.sh`.
