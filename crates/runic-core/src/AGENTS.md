# AGENTS.md

Scope: `crates/runic-core/src/`.

- `SizeClass` only from `SizeClasses`; use `size` / `index_of`; sizes-only declaration generates tables + `index_of`.
- `SizeClasses::class_for(LayoutSpec)`: default-align (`align <= 8`) is size + `CLASS_FOR_SIZE` only — no `PAGE_SIZE` probe; no `lower_bound_index` on that path.
- `Allocator::alloc_zeroed` classifies once then zeros run blocks here; do not call `alloc` (double-classify).
- No `#[cfg(test)]` constructors on production `LayoutSpec` / entity impls.
