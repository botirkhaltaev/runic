# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + `live` own Free/Live. `allocate` is pop only. `extend` threads one page (min 32) onto the freelist; `issued` advances once.
- Remote admission: private claim bitmap after the in-page header (`issued` on `RemoteLine` + `try_set`).
- Domain ops on `Run`: `allocate` / `extend` / `free` (locate + push) / `claim` / `accept`. Hit ignores the `RunFree` outcome / Discard. `is_discardable` + `discard` is miss / accept. Owner DF undefined.
- Embedded `Link<Run>` coalesces by run (one inbox entry for many claims).
- `RunHeap::acquire` (available or take/map) backs `acquire_run`. Available-list: at most once; `push_available` is idempotent; current may be listed; unbind returns current. Run 0↔1 edges update the owning `Heap` atomic; reclaim confirms with `RunHeap::has_live`.
- Geometry lives on `Run`. `locate` is offset from the run base: `OutOfRange` vs interior `InvalidPointer`. Space is `RUN_SIZE`-aligned in a heap-owned map (`MAP_RUNS` spaces).
- Small free / realloc: `Run::header_of` (mask + raw `base` self-check before forming a `Run` pointer). Miss / extent: `PageMap`. Owner-free hit is `current[class]` only.
- `RunConfig` / `RunPolicy` live in `config.rs`. Empty-run `Discard` is `madvise(MADV_DONTNEED)` on the payload only. Maps stay. Payload maps take `Hints` from `AllocatorConfig`. Details: `crates/runic-core/src/heap/run/README.md`.
