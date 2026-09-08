# AGENTS.md

Scope: `crates/runic-core/`.

- Domain `Result`s live here; process abort only via `Allocator::abort`.
- `Process` is mmap storage only; callers use `Allocator::ctx()`.
- Do not grow `Process` into an alloc/free/realloc manager (cold routing stays on `Allocator`).
- Module map: `crates/runic-core/README.md`, `crates/runic-core/src/README.md`.
