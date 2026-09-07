# runic-core/src

Allocator core organized around entities and invariants.

## Modules

- `allocator`: public core facade and abort boundary used by the global wrapper.
- `arena`: mmap slab (`vacant` / `insert` / `remove`; slots never move). Sharing is the caller's lock, not interior atomics. Heap/run/extent metadata; each chunk owns a `Mapping`.
- `config`: allocator and extent retention/reuse configuration.
- `heap`: owner-local heaps, TLS current run, run/extent heaps, `Heaps`, and thread binding.
- `layout`: normalized layout semantics and mapping sizing (`align` as `NonZeroUsize`; `mapping_len` uses `size + align - 1`).
- `memory`: address ranges, mmap ownership, and page-indexed owner lookup.
- `size_class`: one size-class declaration generates lookup tables. `SizeClass` is minted only by `SizeClasses::class_for`. Free-hit geometry lives on `Run` (span + reciprocal).

## Invariant

Every returned pointer must map to exactly one page-map entry. Runs accept only valid block-boundary frees; extents accept only the exact returned pointer.

## Tests

Unit tests live with the owning module when possible. Cross-entity allocator traces live in `../tests/`.
