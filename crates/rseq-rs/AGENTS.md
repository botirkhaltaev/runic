# AGENTS.md

Scope: `crates/rseq-rs/`.

- Safe public API. `unsafe` only `from_raw` and private asm / syscalls.
- One committing store, last. No lock or CAS on the RSEQ hit.
- `LockedStacks` is a distinct type, not a hidden fallback.
- Hit takes `&Thread`. Do not reload `__rseq_offset` / `fs:0` per op.
- No crate-internal `Vec` / `Box` / `HashMap` / `String` / panic / format.
- User Rust is not a critical section. Closures are miss / full / `Unavailable`.
- Thesis: `crates/rseq-rs/ROADMAP.md`. API: `crates/rseq-rs/README.md`.
