# runic-core/src

Module map for the allocator core. See
[Architecture](../../../ARCHITECTURE.md) for process-wide flows.

## Modules

- `allocator`: `Allocator`, abort, and process-wide context.
- `arena`: immovable slots for heap, run, and extent metadata.
- `config`: `AllocatorConfig`, `HugePage`, `Numa`, `Hints`, `Budget`. The `safe` feature is the Safe build.
- `heap`: owner-local heaps, TLS current run, run/extent heaps, `Heaps`, and thread binding.
- `layout`: normalized layout and mapping size.
- `memory`: address ranges, mmap ownership, and page-indexed owner lookup.
- `size_class`: generated size-class lookup tables.

## Invariant

Every live pointer maps to one `PageOwner`. Runs accept block boundaries;
extents accept the exact returned pointer. Raw decoding stays in page-map and
run modules.

## Tests

Unit tests live with their module. Cross-entity traces live in `../tests/`.
