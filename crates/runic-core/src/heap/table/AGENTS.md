# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `HeapDirectory`: lock-free `slot` via published pointers; internal mutex for `acquire` / `retire` / Draining accept / reclaim. `publish` admits Active or cold-falls to Draining.
- `HeapSlot`: sole lifecycle authority (`SlotState` gen+mode+retired+publishers), `Inbox`, `UnsafeCell<Heap>` (Active TLS owner or directory-locked Draining).
- `SlotState` publishers: in-flight Active publish admits (not unpublished TLS batch size — that stays live via `RemotePending`); close Active→Draining preserves count; Release decrement (fail-closed underflow); retire waits Acquire for zero off the mutex.
- `ThreadHeap`: `bind` / `unbind`, owner-local alloc/free, `lookup_owner`, `batch` / `take_batch`. Never-bound freers publish in `Allocator::free_remote` (not `batch`) so Drop cannot strand `RemotePending`. Sticky miss prefers local/OS run acquire before inbox flush.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; remote/abort **after** `with` returns.
- `Inbox::push_batch`: link `last.next` to old head, then CAS `head`; never swap-before-link. `drain` returns a null-terminated walk (single pass).
- Owner-local hot paths and Active publish must not take the directory mutex. Details: `crates/runic-core/src/heap/README.md`.
