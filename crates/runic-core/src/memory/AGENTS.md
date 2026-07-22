# AGENTS.md

Scope: `crates/runic-core/src/memory/`.

- Keep OS mapping lifecycle in `OsMemory` and `Mapping`.
- `Mapping` construction is private to `os.rs`; every `Mapping` must come from `OsMemory::map` so its `(base, len)` always describes a live, uniquely-owned mmap region. Do not widen this constructor's visibility as a shortcut.
- `OsMemory` has no raw `unmap` escape hatch. Anything that owns mmap'd storage must hold a `Mapping` (or a type that owns one) and rely on `Drop` to release it; do not reintroduce `mem::forget`-plus-manual-`munmap` patterns.
- Keep pointer lookup and ownership publication in `PageMap`.
- Preserve `PageOwner` pointer lifetime assumptions: owners stay live until their page-map range is removed.
- `PageMap` has exactly one on-disk representation per published range: every page in a run or extent range gets a direct per-page entry. Do not add a second encoding (e.g. a span/run-length record) that `PageMap` silently falls back to when the primary one is exhausted or unavailable; if a future optimization needs a denser encoding, replace the representation everywhere rather than layering a fallback next to it.
- `publish_run`/`unpublish_run` and `publish_extent`/`unpublish_extent` are kept symmetric on `PageMap` even though `unpublish_run` currently has no production caller: `RunHeap` never removes a live run from its arena because empty-run reclaim is not implemented yet. When empty-run reclaim lands, wire it through `unpublish_run` instead of adding a new removal method.
- `PageOwner` stays a concrete `Run`/`Extent` enum rather than an opaque `NonNull` + kind tag: callers in `allocator.rs`, `heap/run/heap.rs`, and `heap/extent/heap.rs` pattern-match on it and immediately dereference the typed pointer, so erasing the type would only add casts at every call site without removing any duplication. `MapEntry` already carries the kind bit needed for compact on-disk storage; revisit `PageOwner` only if a caller needs to hold page-map results without knowing the arena type.
- `L1Table` is sized for the full 48-bit address space and relies on the OS to lazily back it with physical pages; it is not densified in v0.5. Revisit only with a profile showing L1 reservation or first-touch cost matters.
- Keep unsafe pointer/provenance code narrow and adjacent to safety comments.
- Add page-map tests for overlap, removal, and L2 boundary behavior when changing lookup logic.
