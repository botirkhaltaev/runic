# AGENTS.md

Scope: `crates/runic-core/`.

- Domain `Result`s live here; process abort only via `Allocator::abort`.
- Do not grow `AllocatorInner` into an alloc/free/realloc manager (cold routing stays on `Allocator`).
- Module map: `crates/runic-core/README.md`, `crates/runic-core/src/README.md`.
