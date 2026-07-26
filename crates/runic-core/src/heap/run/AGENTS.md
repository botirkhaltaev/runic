# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via grow-on-demand `Arena<Run>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap) and one-shot `alloc_run` for the locked non-sticky path. Do not reintroduce a `take_or_*` fork or an `alloc_from` pass-through.
- `ThreadHeap::alloc` sticky hit calls `Run::allocate` in-line (symmetric with sticky `Run::free`); empty sticky uses `acquire_alloc` (flush at most once, retry sticky, then `acquire_run` and park).
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- Owner-local Free/Live **authority** is freelist membership (+ bump): bump allocate does **not** store `BlockStates`; freelist allocate clears the Free bit; owner free marks Free then pushes. Do not reintroduce owner Allocated stores on the bump path.
- `BlockStates` is clear / Free / RemotePending (one `AtomicU8` per block in the mapping state tail; zero-filled ⇒ clear). Free bit exists so delayed double-free and `allocated_block_at` fail closed — not a second Free/Live authority. Remote CAS: clear → pending (`claim`); pending → clear (`unclaim`); pending → Free (`accept` before freelist push). Resolve the atom once via `state_unchecked` for a capacity-proven index.
- Owner `RunState.bump` is non-atomic; cold `issued: AtomicUsize` mirrors it on fresh bump so remote `claim` can reject never-issued indices. Keep `allocate_fresh` `#[cold]` / `#[inline(never)]` so bump/issued updates stay off the freelist hot path.
- Freelist head in `RunState` and intrusive payload links are raw `usize` / `FREE_END` (untagged). Do not store raw user pointers in the freelist head, dual-encode with `Option<BlockIndex>`, or add trusted-pointer helper forks. `free` / `claim` / `accept` validate user pointers with `block_at` against cached `payload_base`. Prefer declaring `state` / `payload_base` / `blocks` early for sticky locality under `repr(Rust)` (not an ABI guarantee); keep `issued` cold after `mapping`. `block_shift` is `Option<NonZeroU32>` — never a `0` sentinel.
- Domain free ops: `Run::free` / `accept` return `Result<(), RunError>` (live→freelist / RemotePending→freelist); `claim` (live→RemotePending); `unclaim` (rollback to live). Map `BlockStateError` → `RunError` with `From` / `?`. `RunHeap::{free,accept}` read `Run::is_full()` before the domain op, then share private `finish_free` for available-list bookkeeping. Sticky TLS free calls `Run::free` only (no `was_full` / `finish_free`).
- `Run::has_live_blocks` / `RunHeap::has_live_blocks` are the reclaim model for outstanding small ownership (including remote-pending); do not reintroduce a heap-level alloc side counter.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
