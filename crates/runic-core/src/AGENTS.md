# AGENTS.md

Scope: `crates/runic-core/src/`.

- Put behavior on the entity that owns the data or invariant.
- Keep module boundaries direct: `Heap`, `PageMap`, `RunHeap`, `ExtentHeap`, `RunArena`, `ExtentArena`, `Run`, `Extent`, `OsMemory`, and `SizeClasses` should own their responsibilities.
- Prefer `NonZero*`, `NonNull`, and named-field domain types over sentinel values or ambiguous tuple structs.
- Unsafe blocks must be narrow and adjacent to the safety reasoning.
- Keep owner-local, remote, and central/shared responsibilities separated in type APIs.
- A cache is acceptable only when owned by the entity whose lifecycle makes cached pointers valid.
- Avoid free helper functions for allocator behavior; put allocation, free, cache, and lifecycle operations on the owning entity.
- Avoid result-bag or adapter types unless they encode a real allocator invariant.
- Avoid callback-style helpers for ordinary control flow.
- Add tests beside the module that owns the invariant being changed.
