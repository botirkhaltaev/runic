# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via grow-on-demand `Arena<Run>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap) and one-shot `alloc_run` for the locked non-sticky / `alloc_slow` path. Bound TLS miss uses `acquire_run` + `ThreadHeap::park_run`. Do not reintroduce a `take_or_*` fork or an `alloc_from` pass-through.
- `ThreadHeap::alloc` sticky hit calls `Run::allocate` in-line (symmetric with sticky `Run::free`); miss (cold) flushes at most once, retries sticky, then `acquire_run` and `park_run`. Do not `#[inline]` `ThreadHeap::alloc`.
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- Owner-local Free/Live is freelist membership (+ bump): `allocate` pops/bumps then `live++` with **no** `BlockStates` store. Do not reintroduce owner Free/Allocated atomics on the sticky path.
- Double-free poison on the owner path is freelist-head identity (immediate double-free of the same block). Do not add payload-cookie checks on owner `free` that store-forward against first-byte user writes or false-positive on live user data.
- `BlockStates` is **RemotePending-only** (one `AtomicU8` per block in the mapping state tail after `RUN_SIZE` payload; zero-filled ⇒ clear). Remote CAS: clear → pending (`claim`), pending → clear (`unclaim` / before freelist push on `accept`). Resolve the atom once via `state_unchecked` for a capacity-proven index.
- Owner `RunState.bump` is non-atomic; cold `issued: AtomicUsize` mirrors it on fresh bump so remote `claim` can reject never-issued indices. Keep `allocate_fresh` `#[cold]` / `#[inline(never)]` so bump/issued updates stay off the freelist hot path.
- Freelist head in `RunState` and intrusive payload links are raw `usize` / `FREE_END` (untagged). Do not store raw user pointers in the freelist head, dual-encode with `Option<BlockIndex>`, or add trusted-pointer helper forks. Owner `free` uses `owner_block` (`Result`, containment + shift index + bump / RemotePending / freelist-head poison) — not `Option` `block_at`. Remote `claim` / `accept` / `unclaim` keep `block_at`. Hot layout: `RunState` / `payload_base` / `blocks` first; `issued` stays cold after `mapping`; `block_shift` is `u32` (`0` = non-pow2 multiply path, else `trailing_zeros`).
- Domain free ops: `Run::free` / `accept` return `Result<(), RunError>` (live→freelist / RemotePending→freelist); `claim` (live→RemotePending); `unclaim` (rollback to live). `RunHeap::{free,accept}` read `Run::is_full()` before the domain op, then share private `finish_free` for available-list bookkeeping. Sticky TLS free calls `Run::free` only (no `was_full` / `finish_free`). Do not add TLS magazine push/pop on the owner-local hot path unless `explicit/single_size_churn/runic/64` wall-gates a win vs freelist-primary tip (magazines lost ~25–40% after freelist-primary + page-cache).
- `Run::has_live_blocks` / `RunHeap::has_live_blocks` are the reclaim model for outstanding small ownership (including remote-pending); do not reintroduce a heap-level alloc side counter.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
