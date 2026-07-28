# AGENTS.md

Scope: `crates/runic-core/src/heap/directory/`.

- Files: `mod.rs` (`HeapDirectory`), `state.rs`, `slot.rs` (`HeapSlot`, `LockedSlot`, private `SlotHeap`), `inbox.rs`, `thread.rs`.
- `HeapDirectory`: lock-free `slot`; mutex for `acquire` / `retire` / `lock` → `LockedSlot` / reclaim.
- `HeapSlot`: `SlotState`, inboxes, `UnsafeCell<SlotHeap>`; `enqueue` / `flush` / `free` / `alloc_*` / `acquire_run` via `heap_mut()`.
- `LockedSlot`: exclusive Draining — `enqueue` / `free` / `flush`; Drop → `try_reclaim`.
- `Inbox` / `InboxLink`: `push` / `link` / `drain`; never swap-before-link.
- `SlotState` publishers = in-flight Active enqueue leases (not inbox depth); `PublisherLease` RAII; close preserves count.
- `ThreadHeap`: `bind` / `unbind`; hot `alloc` / `alloc_extent` / `free_run` / `free_extent` / `lookup_owner` take `NonNull<AllocatorInner>` + `&PageMap`. Sticky empty: `#[cold] refill_sticky`.
- Details: `crates/runic-core/src/heap/README.md`.
