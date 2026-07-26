# runic-core/src

Allocator core organized around entities and invariants.

## Modules

- `allocator`: public core facade and abort boundary used by the global wrapper.
- `arena`: grow-on-demand freelist object table for heap/run/extent metadata (each chunk owns a `Mapping`; hard `max`; fixed chunk directory).
- `config`: allocator and extent retention/reuse configuration.
- `heap`: owner-local heaps, run/extent heaps, `HeapDirectory`, and thread binding.
- `layout`: normalized layout semantics and mapping sizing (`align` as `NonZeroUsize`; `mapping_len` uses `size + align - 1`).
- `memory`: address ranges, mmap ownership, and page-indexed owner lookup.
- `size_class`: size-class selection; `SIZES` is the only hand-authored table and the align remap is const-generated from it. `SizeClassId` is a bounded index minted only by `SizeClasses`.

## Invariant

Every returned pointer must map to exactly one page-map entry. Runs accept only valid block-boundary frees; extents accept only the exact returned pointer.

## Tests

Unit tests live with the owning module when possible. Cross-entity allocator traces live in `../tests/`.
