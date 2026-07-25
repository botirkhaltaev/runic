# memory

Memory modules own address ranges, OS mappings, and page-indexed pointer lookup.

## Files

- `address.rs`: ownership-free `AddressRange` geometry and pointer offset checks.
- `os.rs`: `OsMemory::map` and `Mapping` (mmap ownership; `Drop` munmaps).
- `page_map/`: page-indexed lookup from user pointers to `PageOwner` metadata pointers.
  - `mod.rs`: `PageMap` API (`publish_run`, `publish_extent`/`unpublish_extent`, `get`) — publish takes `&Mapping`; writers install L1/L2 once via CAS and stamp pages via `compare_exchange`; `get` is lock-free Acquire loads.
  - `entry.rs`: `MapEntry` / `AtomicMapEntry` tagged-pointer encoding; mutation is CAS only.
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: `L1Table` / `L1Entry` / `L2Table` — once-only L2 install; segment assign/clear with reverse-CAS rollback.
  - `tests.rs`: page-map unit tests.
- `mod.rs`: module exports.

## Invariants

- Every `Mapping` is constructed only by `OsMemory::map`: nonzero page-multiple length, page-aligned base, uniquely owned until `Drop`.
- `AddressRange` does not own mmap lifecycle; it is copyable geometry only.
- Every returned pointer maps to exactly one `PageOwner` while allocated.
- `PageOwner` pointers must refer to live arena entries until their page-map range is removed.
- Page-map insertion rejects overlapping ownership (`empty → owner` CAS failure → `Overlap`).
- Page-map removal validates the expected owner by CAS (`owner → empty`); failed remove restores already-cleared pages.
- Runs and extents share one page-map representation: every page in a published range gets its own direct entry. There is no secondary encoding and no silent fallback between representations.
- L1/L2 mmap ownership is stored by the install CAS winner in `UnsafeCell<Option<Mapping>>` cells; table pointers are published atomically; L2 tables stay for the `PageMap` lifetime. `get` never reads those cells.

## Intentional scope decisions (v0.5)

- No opaque `PageOwner` pointer: it stays a concrete `Run`/`Extent` enum since every caller immediately needs the typed pointer.
- No denser `L1Table`: the table spans the full 48-bit address space and depends on OS lazy paging; not revisited without profiling data.
- No empty-L2 reclaim: once an L2 is installed it remains until `PageMap` drop.
- Consumers that self-host inside a `Mapping` (e.g. `AllocatorInner`) must drop other fields before that `Mapping` munmaps; see `allocator.rs` `Drop`.
