# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- Files: `state.rs` (`HeapMode`, `SlotState`, `PublisherLease`), `slot.rs` (`HeapSlot`, `LockedSlot`), `directory.rs` (`HeapDirectory`), `inbox.rs`, `thread.rs`.
- `HeapDirectory`: lock-free `slot`; mutex for `acquire` / `retire` / `lock` → `LockedSlot` (Draining exclusive) / reclaim.
- `HeapSlot`: lifecycle (`SlotState`), `RunInbox` + `ExtentInbox`, one `UnsafeCell<SlotHeap>` (`id` + `RunHeap` + `ExtentHeap` — no thin public `Heap`); `enqueue` (Active push-or-coalesce; lease only if newly queued), `flush` / `free` / `alloc_*` / `acquire_run` / `return_available` via `heap_mut()`.
- `LockedSlot`: exclusive Draining token — `enqueue` (link already-queued), `free`, `flush`; Drop → `try_reclaim`.
- `Inbox` / `InboxLink` / `InboxNode`: `push` (try_queue+link), `link` (already queued), `drain` (null-terminated walk). Never swap-before-link.
- `SlotState` publishers: in-flight Active enqueue leases (not inbox depth); `acquire_publisher` returns a `PublisherLease` RAII guard; close Active→Draining preserves count; Release decrement (fail-closed underflow); retire waits Acquire for zero off the mutex.
- `HeapError` lives in `heap/error.rs` (not duplicated here).
- `ThreadHeap`: `bind(inner)` / `unbind`, `alloc` / `alloc_extent` / `free_run` / `free_extent`, `lookup_owner(&inner, ptr)`. Hot paths take `&AllocatorInner`. `Allocator::free_cross_heap` claims then `enqueue`s. Sticky miss: local/OS acquire then flush.
- `lookup_owner`: one-entry TLS page→**run** cache only (never extents); clear on unbind; fill only while `matches(inner)`.
- `Allocator::dealloc`: one `THREAD_HEAP.with` for lookup + free; cross-heap/abort **after** `with` returns.
- Owner-local hot paths and Active enqueue must not take the directory mutex. Details: `crates/runic-core/src/heap/README.md`.
