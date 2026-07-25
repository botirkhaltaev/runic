# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- Put exact-pointer, remote-pending, reuse, and live-allocation checks on `Extent`.
- Keep metadata storage on `ExtentHeap` via grow-on-demand `Arena<Extent>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- `ExtentCache` is a bounded index of **published** Free extents (`NonNull<Extent>` into the arena), not raw `Mapping`s. Exact-length reuse only; `ExtentPolicy::{Drop, Keep}` with slot/byte budgets; `Keep` never evicts to admit. Do not reintroduce cache-raw-`Mapping`-after-unpublish.
- `ExtentHeap::free` (owner-local) validates via `Extent::free` then retires; `complete_remote_free` validates remote-pending + exact pointer then retires. Both must share the private `retire` method.
- `retire` Keep path: leave the extent in the arena and page map, finish remote→Free if needed, `cache.insert(extent_ptr)`. Release path (Drop policy / over budget): `unpublish_extent` → arena `remove` → drop mapping. Cache hits in `allocate` call `Extent::reuse` and must not `publish_extent`.
- Same-thread extent allocate/free must go through `ThreadHeap::alloc_extent` / `ThreadHeap::free_extent`, which call `Heap::allocate_extent` / `Heap::free_extent_owner` without taking the table mutex, mirroring the run TLS path. Fall back to `bind` + locked `Heap` only on TLS miss or cross-heap pointers.
- `ExtentHeap::has_live_extents` is Allocated/RemotePending presence (cached Free is not live); pair with `RunHeap::has_live_blocks` for Draining reclaim — no heap-level alloc side counter.
- On heap reincarnation, `ExtentHeap::rebind_heap_id` stamps every occupied arena extent (including cached Free), matching `RunHeap::rebind_heap_id`.
