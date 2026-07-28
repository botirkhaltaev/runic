# AGENTS.md

Scope: `crates/runic-core/src/`.

- `SizeClass` only from `SizeClasses`; use `size` / `index_of`; size tables are generated from the sizes-only declaration.
- `SizeClasses::class_for(LayoutSpec)`: default-align (`align <= 8`) is size + `CLASS_FOR_SIZE` only — no `PAGE_SIZE` probe; no `lower_bound_index` on that path.
- `Allocator::alloc_zeroed` classifies once then zeros run blocks here; do not call `alloc` (double-classify).
- `HeapError` at the slot edge keeps pointer kinds (`InvalidRunPointer` / `InvalidExtentPointer`) and `MissingExtent`.
- No `#[cfg(test)]` constructors or accessors on production `LayoutSpec` / entity `impl` blocks.
