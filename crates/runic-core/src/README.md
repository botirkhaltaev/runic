# runic-core/src

Allocator core organized around entities and invariants.

## Modules

- `allocator`: public core facade and abort boundary used by the global wrapper.
- `arena`: fixed-capacity freelist object table for heap/run/extent metadata.
- `config`: allocator and extent retention/reuse configuration.
- `heap`: owner-local heaps, run/extent heaps, heap table, and thread binding.
- `layout`: normalized layout semantics and mapping sizing.
- `memory`: address ranges, mmap ownership, and page-indexed owner lookup.
- `size_class`: size-class selection.

## Invariant

Every returned pointer must map to exactly one page-map entry. Runs accept only valid block-boundary frees; extents accept only the exact returned pointer.

## Tests

Unit tests live with the owning module when possible. Cross-entity allocator traces live in `../tests/`.
