# AGENTS.md

Scope: `crates/runic-bench/`.

- Internal (`publish = false`). Deterministic workloads; do not change allocator semantics to win a bench.
- Touch allocated memory so work is not optimized away.
- Prefer phase-isolated benches (`owner_free_only`, `freelist_allocate_only`, channel-free prepare/free/accept) when gating free vs alloc vs remote accept.
- After changes: `cargo bench -p runic-bench --no-run`. Cycles/op gates: `scripts/profile.sh` (prefer `--compare`; resolves fresh bench bins).
