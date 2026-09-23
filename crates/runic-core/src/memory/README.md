# memory

Address ranges, OS mappings, and page-indexed lookup. See
[Architecture](../../../../ARCHITECTURE.md#pointer-recovery).

## Files

- `address.rs`: ownership-free `AddressRange` geometry and pointer offset checks.
- `os.rs`: `OsMemory::{map,map_aligned,discard}` and owning `Mapping`.
- `page_map/`: page-indexed lookup from user pointers to process-lifetime `PageOwner` metadata.
  - `mod.rs`: `PageMap::{publish,unpublish,get}` and `PageOwner`.
  - `entry.rs`: `MapEntry` / `AtomicMapEntry` tagged-pointer encoding (`load` / `store`).
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: hot lookup tables, cold write state, and L2 page stamps.
  - `tests.rs`: page-map unit tests.
- `mod.rs`: module exports.

## Invariants

- `OsMemory::{map,map_aligned}` creates every `Mapping`: nonzero page-multiple
  length, page-aligned base, unique ownership until `Drop`.
- `AddressRange` does not own mmap lifecycle; it is copyable geometry only.
- Every returned pointer maps to exactly one `PageOwner` while allocated.
- Layout-known miss/realloc may probe `Run::header_of`; pointer-only operations
  use `PageMap`.
- A `PageOwner` is a process-lifetime `&Run` or immortal `&Extent`. Unmapping
  an extent drops the mapping, not its metadata slot.
- Publish rejects overlaps. Unpublish validates the expected owner before
  clearing and leaves the map unchanged on failure.
- Each published page stores one direct owner entry. `get` reads only the hot
  table pointers and takes no lock.
- Write exclusion is per L2 table. Published L2 tables live until `PageMap`
  drops.
