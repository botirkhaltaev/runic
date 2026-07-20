# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via `Arena<Run>` (`claim` / `insert` / `release` / `remove`).
- Available-run lists are owned by `RunHeap`; sticky TLS caches must return through `return_available`.
- Prefer entity methods over free helpers for allocate/free/remote paths.
