# AGENTS.md

Scope: `crates/runic-core/src/`.

- `SizeClass` only from `SizeClasses`; use `size`. `index_of` is test-only; the free hit uses `Run` span + reciprocal.
- `SizeClasses::class_for(LayoutSpec)`: default-align indexes `CLASS_FOR_SIZE` by size (`0` is class 0). Do not `size.max(align)` or probe `PAGE_SIZE` on that path.
- `Allocator::alloc_zeroed` classifies once then zeros run blocks here; do not call `alloc` (double-classify).
- No `#[cfg(test)]` constructors or accessors on production `LayoutSpec` / entity `impl` blocks.
