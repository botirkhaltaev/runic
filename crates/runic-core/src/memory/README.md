# memory

Memory modules own address ranges, OS mappings, and page-indexed pointer lookup.

## Files

- `address.rs`: ownership-free `AddressRange` geometry and pointer offset checks.
- `os.rs`: `OsMemory::map` and `Mapping` (mmap ownership; `Drop` munmaps).
- `page_map/`: page-indexed lookup from user pointers to `PageOwner` metadata pointers.
  - `mod.rs`: `PageMap` API (`publish_run`, `publish_extent`/`unpublish_extent`, `get`) — publish takes `&Mapping`; write paths lock `l1_mapping` (`Option<Mapping>` for the L1 mmap) while `get` loads `l1` atomically.
  - `entry.rs`: `MapEntry`, the compact tagged-pointer encoding stored per page.
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: `L1Table`/`L1Entry`/`L2Table`/`L2Mapping` — atomic L2 publish for readers; `L2Mapping` holds each L2 mmap and occupancy under `l1_mapping`.
  - `tests.rs`: page-map unit tests.
- `mod.rs`: module exports.

## Invariants

- Every `Mapping` is constructed only by `OsMemory::map`: nonzero page-multiple length, page-aligned base, uniquely owned until `Drop`.
- `AddressRange` does not own mmap lifecycle; it is copyable geometry only.
- Every returned pointer maps to exactly one `PageOwner` while allocated.
- `PageOwner` pointers must refer to live arena entries until their page-map range is removed.
- Page-map insertion rejects overlapping ownership.
- Page-map removal validates the expected owner before clearing entries.
- Runs and extents share one page-map representation: every page in a published range gets its own direct entry. There is no secondary encoding and no silent fallback between representations.
- L1/L2 table mappings are owned as `Option<Mapping>` (`PageMap::l1_mapping` and `L2Mapping`); table pointers are published atomically only after that ownership is stored.

## Intentional scope decisions (v0.5)

- No opaque `PageOwner` pointer: it stays a concrete `Run`/`Extent` enum since every caller immediately needs the typed pointer.
- No denser `L1Table`: the table spans the full 48-bit address space and depends on OS lazy paging; not revisited without profiling data.
- Consumers that self-host inside a `Mapping` (e.g. `AllocatorInner`) must drop other fields before that `Mapping` munmaps; see `allocator.rs` `Drop`.
