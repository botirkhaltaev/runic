# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `ThreadHeap`: `bind` / `unbind`, owner-local `alloc` / `free` / `dealloc` / `alloc_extent` / `free_extent`, `lookup_owner`, `park_run`, `batch` / `take_batch` — no allocate/dealloc routers mirrored from `HeapTable`.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` — `id_for(LayoutSpec::from_layout(layout))` → `ThreadHeap::dealloc` (sticky `Run::free` first; PageMap only on miss/extent/remote); remote/abort **after** `with` returns.
- Empty sticky refill parks via `park_run`; keep one-shot `Heap::alloc_run` off sticky under the table lock.
- `HeapTable`: `acquire` / `retire` / `reclaim`, generation-checked `heap`/`mode`, mode-aware `publish` only.
- Owner-local hot paths must not take the table mutex. Details: `crates/runic-core/src/heap/README.md`.
