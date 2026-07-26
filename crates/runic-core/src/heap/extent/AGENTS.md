# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate exact pointer **before** any state CAS.
- `ExtentCache` indexes published Free arena extents (`NonNull<Extent>`), not raw `Mapping`s after unpublish.
- `ExtentHeap::{free,accept}` → domain op then shared `retire` / `release`.
- Unbound allocate → `Allocator::alloc_extent_remote`; cross-heap free → `Allocator::free_remote`.
- Details: `crates/runic-core/src/heap/extent/README.md`.
