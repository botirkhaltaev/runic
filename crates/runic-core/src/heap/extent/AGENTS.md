# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- Put exact-pointer and remote-pending behavior on `Extent`.
- Keep metadata storage on `ExtentHeap` via `Arena<Extent>` (`claim` / `insert` / `release` / `remove`).
- Extent cache policy lives on `ExtentCache` owned by `ExtentHeap`.
