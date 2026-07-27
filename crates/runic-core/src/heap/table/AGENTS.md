# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `HeapDirectory`: lock-free `slot` via published pointers; internal mutex for `acquire` / `retire` / Draining accept / reclaim. `publish` / `publish_on` admit Active or cold-fall to Draining.
- `HeapSlot`: sole lifecycle authority (`SlotState` gen+mode+retired+publishers), `RunInbox` + `ExtentInbox`, `UnsafeCell<Heap>` (Active TLS owner or directory-locked Draining).
- `SlotState` publishers: in-flight Active publish admits (not inbox depth — that stays live via claim bits / `has_live_allocations`); close Active→Draining preserves count; Release decrement (fail-closed underflow); retire waits Acquire for zero off the mutex.
- `ThreadHeap`: `bind` / `unbind`, owner-local alloc/free, `lookup_owner`. No outbound remote-free state — `Allocator::free_remote` claims and, on `Run`/`Extent::try_arm`, publishes immediately. Sticky miss prefers local/OS run acquire before inbox flush.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; remote/abort **after** `with` returns.
- Callers of `publish`/`publish_on` (`HeapSlot::publish_target`) must have already won `try_arm`; it pushes via `Inbox::republish`, never re-arms. `Inbox::republish` links `next` to old head, then CAS `head`; never swap-before-link. `drain` returns a null-terminated walk (single pass).
- Owner-local hot paths and Active publish must not take the directory mutex. Details: `crates/runic-core/src/heap/README.md`.
