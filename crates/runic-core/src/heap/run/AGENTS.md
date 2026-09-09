# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Freelist + `live` own Free/Live. `allocate` is pop only. `extend` threads one page (min 32) onto the freelist; `issued` advances once.
- Remote admission: private claim bitmap in the space tail (`issued` + `try_set`).
- Domain ops on `Run`: `allocate` / `extend` / `free` (locate + push) / `claim` / `accept`. Owner DF undefined.
- Embedded `InboxLink` coalesces by run (one inbox entry for many claims).
- `RunHeap::acquire` (available or take/map) backs `acquire_run`. Available-list: at most once; `push_available` is idempotent; current may be listed; unbind returns current.
- Geometry lives on `Run`. `locate` is offset from the run base: `OutOfRange` vs interior `InvalidPointer`. Space is `RUN_SIZE`-aligned in a heap-owned map (`MAP_RUNS` spaces).
- `RunCache` owns the TLS miss/realloc probe (`hit` / `store` / `clear`). `lookup` is `RunCache` → `current[class]` → `PageMap`. Owner-free hit is `current[class]` only.
- `RunConfig` / `RunPolicy` live in `config.rs`. Empty-run `Discard` is `madvise(MADV_DONTNEED)` on the payload only. Maps stay. Details: `crates/runic-core/src/heap/run/README.md`.
