# AGENTS.md

Scope: `crates/rseq-rs/`.

- Safe public API. `unsafe` only `from_raw` and private asm / syscalls.
- Primitive is `Thread` + `Word`. `Words` is an optional mmap. No ops on `Words`.
- One glibc `struct rseq` per thread. User-space writes `rseq_cs` only.
- `Word.cpu` confirms `area.cpu_id`. Do not pre-read or overlay `cpu_id_start`.
- One committing store, last. CS aborts if `area.cpu_id != word.cpu`.
- No lock or CAS on the RSEQ hit. No locked twin.
- Hit takes `&Thread`. Do not reload `__rseq_offset` / `fs:0` per op.
- One attempt. Kernel preemption restarts inside the CS. CPU mismatch is `Error::Abort`.
- `try_new` is glibc rseq + CPU count. `fence` is membarrier RSEQ; word ops never fence.
- Isolated TLS winning a pinned increment is not a bug (`#135`).
- This crate never uses the global allocator. `mmap` is the OS boundary. Cold `try_new` may use `OnceLock` and `File` into a stack buffer.
- User Rust is not a critical section. `try_new` is `None` → caller `AtomicUsize`.
- Thesis: `crates/rseq-rs/ROADMAP.md`. API: `crates/rseq-rs/README.md`.
