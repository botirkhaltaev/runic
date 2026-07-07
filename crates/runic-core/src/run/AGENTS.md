# AGENTS.md

Scope: `crates/runic-core/src/run/`.

- Keep run block-state behavior on `Run` and `FreeBitmap`.
- Keep small-allocation policy and available-run list behavior on `RunHeap`.
- Keep empty-run retention behavior on `RunCache`.
- Keep metadata storage and reservation behavior on `RunArena`.
- Do not add caches that make a block reachable from two owners at once.
- Preserve block-boundary validation and double-free detection.
- Add tests beside the run entity that owns the changed invariant.
