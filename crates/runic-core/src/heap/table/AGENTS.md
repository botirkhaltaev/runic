# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- Keep TLS heap-entry owner-local frontend state and run caches here.
- `Inbox` is a movable Treiber-style head of intrusive `RemoteList` batches; construct with `Inbox::new()`.
- `RemoteList.first`/`.last` are plain `NonNull<u8>`, not `Option`: a list is only ever built from a non-empty batch, so construction (`RemoteList::from_ends`) and `Inbox::push_batch` never need to check or `expect` non-emptiness.
- Keep `ThreadHeap` thin and composable: `bind` / `unbind`, owner-local `alloc` / `free` (runs) and `alloc_extent` / `free_extent` (extents), `bound`, `lookup_owner`, and remote `batch` / `take_batch`. Sticky hit is the straight-line body of `alloc` / `free`; miss paths are cold. `lookup_owner` is a one-entry TLS page→`PageOwner` cache in front of `PageMap::get` (miss still hits PageMap; clear on unbind) — not a PageMap skip. `Allocator::dealloc` must enter `THREAD_HEAP.with` once for lookup + `free` / `free_extent` (no nested `with` after lookup). Do not mirror `Heap`/`HeapTable` allocate-dealloc routers on TLS. Extents have no sticky TLS slot cache; mapping reuse stays on `ExtentCache`.
- Bound-heap access from TLS uses `bound_heap() -> NonNull<Heap>` plus local `as_mut()` at the call site; do not add `&mut Heap` from `&self` helpers that need `clippy::mut_from_ref` expects.
- `ThreadHeap::bind` reuses a matching binding or unbinds a foreign one then `HeapTable::acquire`s; `unbind` returns cached runs, publishes outbound batches, and `retire`s the bound heap.
- Remote frees use batched transport: `ThreadHeap::batch` coalesces onto `RemoteBatch`; callers `HeapTable::publish` returned lists. Do not add single-node push/pop façades.
- Create heaps with `Heap::new` + grow-on-demand `Arena::claim` / `insert` (inbox is movable; no placement-only install). `HEAP_METADATA_CAPACITY` is a hard max for run/extent arenas, not a pre-touch size. Arena chunks each own a `Mapping` (no `mem::forget` / raw munmap).
- Keep `Heap` responsible for Free/Active/Draining mode and owner-local lifecycle helpers; `Heap::mode()` returns the `HeapMode` snapshot directly (Free/Active/Draining) for callers that must branch on lifecycle state.
- Keep `HeapTable` thin and composable: `acquire` / `retire` / `reclaim`, generation-checked `heap` / `heap_mut` / `mode`, and mode-aware `publish`. Do not put allocate/dealloc routers on the table.
- `HeapTable::acquire` returns `(HeapId, NonNull<Heap>)` for bind; `heap`/`heap_mut`/`mode` fail closed on stale generations.
- `HeapTable::publish` under the table lock: `Active` enqueues to the inbox; `Draining` enqueues then `flush`es and may `reclaim`; `Free`/stale generation fails. Retained TLS batches must stay publishable after owner exit.
- Do not put `HeapTable` on steady-state owner-local allocation hot paths (`ThreadHeap::alloc` / `alloc_extent` / owner-local `free` / `free_extent` must not take the table mutex).
- Owner non-cached free is `Heap::free(PageOwner)` (may flush inbox); TLS sticky run free stays `Run::free` only (live ownership lives on the run; no `was_full` / `finish_free`). Sticky cell pointer-eq runs before `matches(inner)` / `HeapId` (sticky slots only park this heap's runs and are cleared on unbind, so sticky hit implies bound); sticky/domain free errors and `Allocator::dealloc` non-local arms are `#[cold]`.
- Clear or validate owner-local caches whenever a heap is abandoned or reactivated.
- Preserve explicit separation between owner-local frees and remote-free claim→`batch`→`publish`→drain behavior. There is exactly one remote-free protocol (`Allocator::free_remote` claims, coalesces via `ThreadHeap::batch`, and calls `publish`; draining completion under the table lock uses `Heap::flush` → `runs`/`extents` `accept` + `reclaim`). Do not add a second, unbatched remote-free implementation for `realloc` or any other caller — route all cross-heap frees (including from `realloc`) through the same `Allocator::dealloc` (one TLS entry → `ThreadHeap::free` / `free_extent`) / `Allocator::free_remote` path.
- Do not introduce passive forwarding wrappers for heap table behavior; prefer methods on `HeapTable`, `Heap`, or `ThreadHeap` that owns the state.
- Fatal TLS publish/retire failures call `Allocator::abort` — the single crate abort sink — not a local `abort()` helper.
