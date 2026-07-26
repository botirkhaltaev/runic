# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- Put exact-pointer checks, state transitions, reuse, and live-allocation queries on `Extent`.
- `Extent::{free, claim, accept}` validate the exact returned pointer **before** any state CAS (mirror `Run::claim`). Do not transition then validate.
- Keep metadata storage on `ExtentHeap` via grow-on-demand `Arena<Extent>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- `ExtentCache` is a bounded index of **published** Free extents (`NonNull<Extent>` into the arena), not raw `Mapping`s. Exact-length reuse only; `ExtentPolicy::{Drop, Keep}` with slot/byte budgets; `Keep` never evicts to admit. Do not reintroduce cache-raw-`Mapping`-after-unpublish.
- `ExtentHeap::free` calls `Extent::free` then `retire`; `accept` calls `Extent::accept` then `retire`. Both share private `retire` / `release`.
- `retire` assumes the extent is already Free. Keep path: leave arena + page map, `cache.insert`. Release path (Drop / over budget / insert miss): `unpublish_extent` → arena `remove` → drop mapping. Cache hits call `Extent::reuse` and must not `publish_extent`.
- Same-thread extent allocate/free must go through `ThreadHeap::alloc_extent` / `ThreadHeap::free_extent`, which call `Heap::allocate_extent` / `Heap::free(PageOwner::Extent)` without taking the table mutex, mirroring the run TLS path. Fall back to `Allocator::alloc_extent_remote` (bind + locked `Heap`) when unbound, or `Allocator::free_remote` for cross-heap pointers.
- `ExtentHeap::has_live_extents` is Allocated/RemotePending presence (cached Free is not live); pair with `RunHeap::has_live_blocks` for Draining reclaim — no heap-level alloc side counter.
- On heap reincarnation, `ExtentHeap::rebind_heap_id` stamps every occupied arena extent (including cached Free), matching `RunHeap::rebind_heap_id`.
