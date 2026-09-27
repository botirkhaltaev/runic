# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- `Freelist` + `live` own Free/Live. `allocate` pops. `extend` pushes one page (min 32) of fresh blocks; `issued` advances once. Safe free asks `Freelist::ensure_absent`.
- Remote admission: `claim` checks `issued`. `safe` also `try_set`s a claim bit, private to `claim` and `accept`. A second claim is undefined on Fast.
- `ThreadHeaps::hold` links claimed blocks on slots (8 slots, chains of 16). The front slot is the run just freed. A full chain, or the fullest when every slot is in use, calls `Run::push`. `Heap::enqueue` runs when the inbox link is idle. `accept` stores idle and splices the chain. `safe` clears those bits.
- Domain ops on `Run`: `allocate` / `extend` / `free` (locate + push) / `claim` / `push` / `accept`. Hit ignores the `RunFree` outcome / Discard. Slow free uses `RunHeap::release` for the available list and Discard. Owner DF undefined.
- Embedded `Link<Run>` is one atomic. Idle is a sentinel. `queue` CASes idle, then links. Coalesce by run.
- `RunHeap::acquire` is available or take/map. A miss takes an available run from any attached heap, then `acquire`s on the first heap that can map. Available-list: at most once; `push_available` is idempotent; current may be listed; unbind returns current. Run 0↔1 edges update the owning `Heap` atomic; reclaim confirms with `RunHeap::has_live`.
- Geometry lives on `Run`. `locate` is offset from the run base: `OutOfRange` vs interior `InvalidPointer`. Space is `RUN_SIZE`-aligned in a heap-owned map (`MAP_RUNS` spaces).
- Small free / realloc: `Run::header_of` (mask + raw `base` self-check before forming a `Run` pointer). Miss / extent: `PageMap`. Owner-free hit is `current[class]` only.
- `RunConfig` / `RunPolicy` live in `config.rs`. Empty-run `Discard` is `madvise(MADV_DONTNEED)` on the payload only. Maps stay. Payload maps take `Hints` from `AllocatorConfig`. Details: `crates/runic-core/src/heap/run/README.md`.
