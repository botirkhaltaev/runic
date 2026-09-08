# AGENTS.md

Scope: `crates/runic-core/tests/`.

- Cross-entity behavior only; module-private invariants stay beside the owning module.
- Aborting invalid frees → subprocess tests in `crates/runic/tests/`.
- Do not revive TLS-batch freer narratives; claim→enqueue is immediate.
- Run-retention traces that install process state belong in `run_reuse.rs` (own process).
