# AGENTS.md

Scope: `crates/runic-core/src/heap/extent/`.

- `Extent::{free,claim,accept}` validate the exact pointer before any state write. Owner `free` does not detect DF.
- `ExtentConfig` / `ExtentPolicy` live in `config.rs`. `ExtentCache` is a `LinkedList` of published Free arena extents (`slot: list::Link`). `Budget::slots` is exact. Payload maps take `Hints` from `AllocatorConfig`.
- `ExtentHeap::free` then `cache_or_unmap` (`insert` or `unmap`). Allocate/free edges update the owning `Heap` atomic; reclaim confirms with `ExtentHeap::has_live`. Extent slots are immortal; unmap drops only `Mapping`. `inbox` is the remote-free link; `slot` is the cache or unmapped list.
- Keep/Discard retention never evicts a cached extent to admit another; reuse is exact mapping length only. `Extent::discard` is Discard cache-insert `madvise` only. Keep Zeroed reuse ≥64 KiB discards pages without that flag, else memset. `resize_in_place` refuses a size-class spec.
- Details: `crates/runic-core/src/heap/extent/README.md`.
