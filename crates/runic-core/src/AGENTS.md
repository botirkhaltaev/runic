# AGENTS.md

Scope: `crates/runic-core/src/`.

- Put behavior on the entity that owns the data or invariant.
- Keep module boundaries direct: `Heap`, `PageMap`, `RunHeap`, `ExtentHeap`, `Arena`, `Run`, `Extent`, `OsMemory`, and `SizeClasses` should own their responsibilities.
- Prefer `NonZero*`, `NonNull`, and named-field domain types over sentinel values or ambiguous tuple structs.
- `SizeClassId` is a bounded index into `SizeClasses::SIZES`; only `SizeClasses` constructs it. Prefer `SizeClasses::block_size(id)` over result-bag wrappers. Do not hand-maintain a second align-size table — const-generate the align remap from `SIZES`.
- `SizeClasses::id_for(Layout)` keeps default-align (`align <= 8`) as the hot fall-through: small-max check + `CLASS_FOR_SIZE` only (no `PAGE_SIZE` probe on that path). Over-aligned / large-align remap is cold. Do not reintroduce a second bounds check via `lower_bound_index` on the default-align path. `Allocator::alloc` / `alloc_zeroed` classify from `Layout` before building `LayoutSpec` (spec is extent-only).
- `Allocator::alloc_zeroed` classifies once, then allocates (run or extent) and zeros run blocks at this boundary; do not call `alloc` from `alloc_zeroed` (that double-classifies).
- Do not put `#[cfg(test)]` constructors on production `LayoutSpec` / entity impls; tests build specs via `Layout` + `from_layout`.
- Unsafe blocks must be narrow and adjacent to the safety reasoning.
- Keep owner-local and remote-free responsibilities separated in type APIs; there is no central/root ownership heap.
- A cache is acceptable only when owned by the entity whose lifecycle makes cached pointers valid.
- Avoid free helper functions for allocator behavior; put allocation, free, cache, and lifecycle operations on the owning entity.
- Do not add pass-through methods that only forward to another method; call the owning entity directly.
- Avoid result-bag or adapter types unless they encode a real allocator invariant.
- Avoid callback-style helpers for ordinary control flow.
- Add tests beside the module that owns the invariant being changed.
- When this subtree's architecture, APIs, or invariants change, revamp the nearest `AGENTS.md` in the same pass; update `README.md` when those docs would otherwise drift.
