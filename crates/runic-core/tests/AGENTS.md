# AGENTS.md

Scope: `crates/runic-core/tests/`.

- Public `Allocator` behavior only; module-private invariants stay beside the owning module.
- One file per behavior area (`api`, `threads`, `stress`); shared helpers in `common/mod.rs` and nothing unused there, since every file is its own crate.
- Fill and check every block with `Block` before it changes hands; never assert on which internal path ran.
- Aborting invalid frees → subprocess tests in `crates/runic/tests/`.
- Do not revive TLS-batch freer narratives; claim→enqueue is immediate.
