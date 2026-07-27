# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate exact pointer **before** any state CAS.
- `ExtentCache` indexes published Free arena extents (`NonNull<Extent>`), not raw `Mapping`s after unpublish.
- `ExtentHeap::{free,accept}` → domain op then shared `cache_or_unmap` (which calls `unmap` on a cache miss/over-budget).
- Unbound allocate → `Allocator::alloc_unbound`; cross-heap free → `Allocator::free_cross_heap`.
- Details: `crates/runic-core/src/heap/extent/README.md`.
