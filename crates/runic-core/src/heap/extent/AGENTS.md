# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- Put exact-pointer and remote-pending behavior on `Extent`.
- Keep metadata storage on `ExtentHeap` via `Arena<Extent>` (`claim` / `insert` / `release` / `remove`).
- Extent cache policy lives on `ExtentCache` owned by `ExtentHeap`; it exposes exactly `ExtentPolicy::{Drop, Keep}` with exact-length reuse. Do not add best-fit, size-bucket, or eviction-order variants without policy-grid evidence.
- `ExtentHeap::free` (owner-local) validates via `Extent::free` then retires; `complete_remote_free` validates remote-pending + exact pointer then retires. Both must share the private `retire` method; do not re-duplicate unpublish/remove/cache-insert logic between them.
- Same-thread extent allocate/free must go through `ThreadHeap::alloc_extent` / `ThreadHeap::free_extent`, which call `Heap::allocate_extent` / `Heap::free_extent_owner` without taking the table mutex, mirroring the run TLS path. Fall back to `bind` + locked `Heap` only on TLS miss or cross-heap pointers.
