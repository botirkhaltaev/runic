# AGENTS.md

Scope: `crates/rseq-rs/`.

- Safe public API. `unsafe` only `from_raw` and private asm / syscalls.
- Primitive is `Thread` + `Word`. `Words` is an optional mmap. No ops on `Words`.
- One committing store, last. CS aborts if `area.cpu_id != word.cpu`.
- No lock or CAS on the RSEQ hit. No locked twin.
- Hit takes `&Thread`. Do not reload `__rseq_offset` / `fs:0` per op.
- `try_new` is glibc rseq + CPU count. `fence` is optional and registers membarrier itself.
- This crate never uses the global allocator. `mmap` is the OS boundary. Cold `try_new` may use `OnceLock` and `File` into a stack buffer.
- User Rust is not a critical section. Closures are compare-miss / `Unavailable`.
- Thesis: `crates/rseq-rs/ROADMAP.md`. API: `crates/rseq-rs/README.md`.
