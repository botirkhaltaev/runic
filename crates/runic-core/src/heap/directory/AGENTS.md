# AGENTS.md

Scope: `crates/runic-core/src/heap/directory/`.

- Layout: `mod.rs` (`HeapDirectory`), `state.rs`, `slot.rs` (`HeapSlot` / `LockedSlot` / private `SlotHeap`), `inbox.rs`, `thread.rs`.
- `HeapDirectory`: lock-free `slot`; mutex only for `acquire` / `retire` / `lock` → `LockedSlot` / reclaim.
- `HeapSlot`: lifecycle + inboxes + `heap_mut()`; `enqueue` / `flush` / `free` / `alloc_*` / `acquire_run`.
- `LockedSlot`: exclusive Draining token (`HeapId` + lifecycle guard); Drop → `try_reclaim`.
- `Inbox` / `InboxLink`: `push` / `link` / `drain`; never swap-before-link.
- `SlotState` publishers = in-flight Active enqueue leases only; `PublisherLease` RAII; close preserves count.
- `ThreadHeap`: `bind` / `unbind`; sticky empty → `#[cold] refill_sticky` (local/OS then flush). Owner-local hot paths and Active enqueue must not take the directory mutex.
- Details: `crates/runic-core/src/heap/README.md`.
