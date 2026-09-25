# memory

Address ranges, OS mappings, and page-indexed lookup. See
[Architecture](../../../../ARCHITECTURE.md#pointer-recovery).

## Files

- `address.rs`: ownership-free `AddressRange` geometry and pointer offset checks.
- `os.rs`: `Memory` trait (`page_size`, map / map_aligned, discard, and
  payload defaults), the `Linux` impl, and owning `Mapping` (`prefer` /
  `prefer_huge` / `prefer_local`).
- `page_map/`: page-indexed lookup from user pointers to process-lifetime `PageOwner` metadata.
  - `mod.rs`: `PageMap::{publish,unpublish,get}` and the `PageOwner` stamp. `usable` and `resize_in_place` live on the heap.
  - `entry.rs`: `MapEntry` / `AtomicMapEntry` tagged-pointer encoding (`load` / `store`).
  - `page.rs`: page/index arithmetic and per-L1-table range segmentation.
  - `table.rs`: hot lookup tables, cold write state, and L2 page stamps.
  - `tests.rs`: page-map unit tests.
- `mod.rs`: module exports.

## Invariants

- Callers map through the `Os` alias (`mod.rs`), never a named OS. `Linux` is
  the only impl; all OS calls stay in `os.rs` (`Mapping::drop` owns `munmap`).
- `Os::{map,map_aligned}` creates metadata `Mapping`s (process, arena, page-map
  tables): nonzero page-multiple length, page-aligned base, unique ownership
  until `Drop`. Payload maps go through `map_payload` / `map_aligned_payload`,
  which map ordinary pages then `Mapping::prefer`. Hints are independent
  best-effort THP and NUMA-local preferences on the mapping.
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
