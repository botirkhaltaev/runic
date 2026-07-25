# memory

Memory modules own address ranges, OS mappings, and page-indexed pointer lookup.

## Files

- `address.rs`: ownership-free `AddressRange` geometry and pointer offset checks.
- `os.rs`: `OsMemory::map` and `Mapping` (mmap ownership; `Drop` munmaps).
- `page_map/`: page-indexed lookup from user pointers to `PageOwner` metadata pointers.
  - `mod.rs`: `PageMap` API (`publish_run`, `publish_extent`/`unpublish_extent`, `get`) — publish takes `&Mapping`; once-only L1/L2 CAS install; stamps under `L1WriteGuard`; lock-free `get` walks L1 tip → dense L2 tip → page entry.
  - `entry.rs`: `MapEntry` / `AtomicMapEntry` tagged-pointer encoding (`load` / `store`).
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: `L1Table` (hot `tables` + cold `mappings`), `L1WriteGuard`, `L2Table` (pages + per-L2 write exclusion) — install, RAII stamp lock, owner lookup, segment match/write.
  - `tests.rs`: page-map unit tests (including concurrent publish smoke).
- `mod.rs`: module exports.

## Invariants

- Every `Mapping` is constructed only by `OsMemory::map`: nonzero page-multiple length, page-aligned base, uniquely owned until `Drop`.
- `AddressRange` does not own mmap lifecycle; it is copyable geometry only.
- Every returned pointer maps to exactly one `PageOwner` while allocated.
- `PageOwner` pointers must refer to live arena entries until their page-map range is removed.
- Page-map insertion rejects overlapping ownership (validate under `L1WriteGuard`, then store; Drop unlocks).
- Page-map removal validates the expected owner under `L1WriteGuard` before clearing; failed remove leaves the map unchanged.
- Runs and extents share one page-map representation: every page in a published range gets its own direct entry. There is no secondary encoding and no silent fallback between representations.
- L1 root split: `get` indexes only the dense `tables` pointer array; L2 mmap ownership lives in parallel `mappings`. Stamp writers take per-L2 exclusion on `L2Table::write` (installed L2 only). `get` never reads mapping cells or takes write locks.

## Intentional scope decisions (v0.5)

- No opaque `PageOwner` pointer: it stays a concrete `Run`/`Extent` enum since every caller immediately needs the typed pointer.
- No further L1 densify beyond hot tips + cold mappings: the table spans the full 48-bit address space and depends on OS lazy paging; VA shape not revisited without profiling data.
- No empty-L2 reclaim: once an L2 is installed it remains until `PageMap` drop.
- No global PageMap stamp mutex: exclusion is per L2 so disjoint address regions can publish concurrently.
- Consumers that self-host inside a `Mapping` (e.g. `AllocatorInner`) must drop other fields before that `Mapping` munmaps; see `allocator.rs` `Drop`.
