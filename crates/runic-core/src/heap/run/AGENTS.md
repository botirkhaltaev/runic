# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Put block-state and free-list behavior on `Run`.
- Keep metadata storage on `RunHeap` via grow-on-demand `Arena<Run>` (`claim` / `insert` / `release` / `remove`; hard max; each chunk owns a `Mapping` in a fixed directory).
- Available-run lists are owned by `RunHeap`; sticky TLS caches park at most one run per class off those lists and must return through `return_available`.
- Small alloc composition lives on `Heap`: `acquire_run` (flush once if needed, then available or cold mmap) and one-shot `alloc_run` for the locked non-sticky path. Do not reintroduce a `take_or_*` fork or an `alloc_from` pass-through.
- `ThreadHeap::alloc` sticky hit calls `Run::allocate` in-line (symmetric with sticky `Run::free`); miss (cold) flushes at most once, retries sticky, then `acquire_run` and parks the run.
- On heap reincarnation, `RunHeap::rebind_heap_id` stamps every occupied arena run (not only `available[]`), so sticky/off-list runs cannot keep a stale `HeapId`.
- Owner-local Free/Live is freelist membership (+ bump): `allocate` pops/bumps then `live++` with **no** `BlockStates` store; `free` / `accept` push a **tagged** intrusive link in the block payload (double-free / already-free when the tag is present). Do not reintroduce owner Free/Allocated atomics on the sticky path.
- `BlockStates` is **RemotePending-only** (one `AtomicU8` per block in the mapping state tail after `RUN_SIZE` payload; zero-filled ⇒ clear). Remote CAS: clear → pending (`claim`), pending → clear (`unclaim` / before freelist push on `accept`). Resolve the atom once via `state_unchecked` for a capacity-proven index.
- `bump` is `AtomicUsize` so remote `claim` can reject never-issued indices without an Allocated bit; owner allocate/free still own bump updates.
- Freelist head in `RunState` is raw `usize` / `FREE_END` (untagged). Payload links use tagged encoding (`encode_free_next` / `decode_free_next`); pop clears the tag so the block is live. Do not store raw user pointers in the freelist head, dual-encode with `Option<BlockIndex>`, or add trusted-pointer helper forks. `free` / `claim` / `accept` validate user pointers with `block_at` against cached `payload_base`. Hot layout: `RunState` / `bump` / `payload_base` / `blocks` first; `block_shift` is `u32` (`0` = non-pow2 multiply path, else `trailing_zeros`).
- Domain free ops: `Run::free` / `accept` return `Result<(), RunError>` (live→freelist / RemotePending→freelist); `claim` (live→RemotePending); `unclaim` (rollback to live). `RunHeap::{free,accept}` read `Run::is_full()` before the domain op, then share private `finish_free` for available-list bookkeeping. Sticky TLS free calls `Run::free` only (no `was_full` / `finish_free`).
- `Run::has_live_blocks` / `RunHeap::has_live_blocks` are the reclaim model for outstanding small ownership (including remote-pending); do not reintroduce a heap-level alloc side counter.
- Runs are retained in v0.5: no empty-run PageMap unpublish or OS release.
- Prefer entity methods over free helpers for allocate/free/remote paths.
