# AGENTS.md

Scope: `crates/runic-core/src/`.

- `SizeClassId` only from `SizeClasses`; use `block_size(id)` / `non_power_of_two_block_index_from_offset`; derive constant-divisor indexing and lookup tables from the single size-class declaration.
- `SizeClasses::id_for(LayoutSpec)`: default-align (`align <= 8`) is size + `CLASS_FOR_SIZE` only — no `PAGE_SIZE` probe; no `lower_bound_index` on that path.
- `Allocator::alloc_zeroed` classifies once then zeros run blocks here; do not call `alloc` (double-classify).
- No `#[cfg(test)]` constructors on production `LayoutSpec` / entity impls.
