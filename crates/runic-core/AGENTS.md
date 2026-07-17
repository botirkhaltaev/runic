# AGENTS.md

Scope: `crates/runic-core/`.

- This crate owns allocator correctness invariants. Prefer small entity methods over free helpers.
- Keep unsafe code local, explicit, and justified by the owning type's invariant.
- Do not introduce allocator-internal `Vec`, `Box`, `HashMap`, `String`, formatting, or panic paths unless recursion risk is addressed.
- Core APIs should return domain errors; abort policy belongs at the allocator boundary.
- Performance optimizations must preserve explicit allocator ownership; reshape APIs instead of adding compatibility paths.
- Owner-local hot paths may avoid locks only when ownership and remote synchronization are documented in the owning type.
- Preserve `#![deny(unsafe_op_in_unsafe_fn)]` and avoid broad lint allowances.
- Run `cargo test -p runic-core` after core changes; run workspace clippy before commit.
