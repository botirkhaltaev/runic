# AGENTS.md

Scope: `crates/runic-core/`.

- Domain `Result`s here; abort only via `Allocator::abort`.
- Do not grow `AllocatorInner` into an alloc/free/realloc manager.
- Module layout: `crates/runic-core/README.md` and `crates/runic-core/src/README.md`.
