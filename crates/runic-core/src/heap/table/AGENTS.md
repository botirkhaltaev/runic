# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `HeapDirectory`: `acquire` / `retire` / `reclaim` / `publish`; generation-checked `slot` / `slot_mut` only.
- `HeapSlot`: sole lifecycle authority (`HeapRoute` gen+mode+retired), `Inbox`, `publishers` (PR5), owner `Heap` metadata.
- `ThreadHeap`: `bind` / `unbind`, owner-local alloc/free, `lookup_owner`, `batch` / `take_batch` — TLS caches `NonNull<HeapSlot>`.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; remote/abort **after** `with` returns.
- `Inbox::push_batch`: link `last.next` to old head, then CAS `head`; never swap-before-link.
- Owner-local hot paths must not take the directory mutex. Details: `crates/runic-core/src/heap/README.md`.
