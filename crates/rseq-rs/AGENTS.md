# AGENTS.md

Scope: `crates/rseq-rs/`.

- Safe public API. `unsafe` only `from_raw` and private asm / syscalls.
- One committing store, last. No lock or CAS on the RSEQ hit.
- No locked twin. `try_new` is `None` → caller uses `AtomicUsize`.
- Hit takes `&Thread`. Do not reload `__rseq_offset` / `fs:0` per op.
- This crate never uses the global allocator. `mmap` is the OS boundary. Cold `try_new` may use `OnceLock` and `File` into a stack buffer.
- User Rust is not a critical section. Closures are compare-miss / `Unavailable`.
- Thesis: `crates/rseq-rs/ROADMAP.md`. API: `crates/rseq-rs/README.md`.
