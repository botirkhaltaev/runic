# memory

Memory modules own address ranges, OS mappings, and page-indexed pointer lookup.

## Files

- `address.rs`: address ranges and pointer offset checks.
- `os.rs`: mmap/munmap ownership (`Mapping`) and page-size helpers (`OsMemory`).
- `page_map/`: page-indexed lookup from user pointers to `PageOwner` metadata pointers.
  - `mod.rs`: `PageMap` public API (`publish_run`/`unpublish_run`, `publish_extent`/`unpublish_extent`, `get`) and insert/remove orchestration.
  - `entry.rs`: `MapEntry`, the compact tagged-pointer encoding stored per page.
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: `L1Table`/`L1Entry`/`L2Table`, the two-level table storage.
  - `tests.rs`: page-map unit tests.
- `mod.rs`: module exports.

## Invariants

- Every `Mapping` is constructed only by `OsMemory::map`, so its `(base, len)` always describes a live, uniquely-owned mmap region; there is no public constructor that can fabricate one.
- Every returned pointer maps to exactly one `PageOwner` while allocated.
- `PageOwner` pointers must refer to live arena entries until their page-map range is removed.
- Page-map insertion rejects overlapping ownership.
- Page-map removal validates the expected owner before clearing entries.
- Runs and extents share one page-map representation: every page in a published range gets its own direct entry. There is no secondary encoding and no silent fallback between representations.

## Intentional scope decisions (v0.5)

- No opaque `PageOwner` pointer: it stays a concrete `Run`/`Extent` enum since every caller immediately needs the typed pointer.
- No denser `L1Table`: the table spans the full 48-bit address space and depends on OS lazy paging; not revisited without profiling data.
- `AllocatorCore` is self-hosted: it is constructed inside the `Mapping` it stores as its own last field, so dropping it in place unmaps its own backing memory through ordinary field-drop order. There is no raw `OsMemory::unmap` call left in the crate.
