# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- `HeapDirectory`: lock-free `slot`; mutex for `acquire` / `retire` / `lock` → `LockedSlot` (Draining exclusive) / reclaim.
- `HeapSlot`: lifecycle (`SlotState`), `RunInbox` + `ExtentInbox`, `UnsafeCell<Heap>`; `enqueue` (Active push-or-coalesce; lease only if newly queued), `flush` / `free` / `alloc_*`.
- `LockedSlot`: exclusive Draining token — `enqueue` (link already-queued), `free`, `flush`; Drop → `try_reclaim`.
- `Inbox` / `InboxLink` / `InboxNode`: `push` (try_queue+link), `link` (already queued), `drain` (null-terminated walk). Never swap-before-link.
- `SlotState` publishers: in-flight Active enqueue leases (not inbox depth); close Active→Draining preserves count; Release decrement (fail-closed underflow); retire waits Acquire for zero off the mutex.
- `ThreadHeap`: `bind` / `unbind`, owner-local alloc/free, `lookup_owner`. No outbound remote-free state — `Allocator::free_remote` claims then `enqueue`s. Sticky miss prefers local/OS run acquire before inbox flush.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; remote/abort **after** `with` returns.
- Owner-local hot paths and Active enqueue must not take the directory mutex. Details: `crates/runic-core/src/heap/README.md`.
