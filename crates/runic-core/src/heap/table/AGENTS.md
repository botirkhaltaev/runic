# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `ThreadHeap`: `bind` / `unbind`, owner-local alloc/free, `lookup_owner`, `batch` / `take_batch` — no allocate/dealloc routers mirrored from `HeapTable`.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; remote/abort **after** `with` returns.
- `HeapTable`: `acquire` / `retire` / `reclaim`, generation-checked `heap`/`mode`, mode-aware `publish` only.
- Owner-local hot paths must not take the table mutex. Details: `crates/runic-core/src/heap/README.md`.
