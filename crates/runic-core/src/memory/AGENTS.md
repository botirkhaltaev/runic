# AGENTS.md

Scope: `crates/runic-core/src/memory/`.

- Keep OS mapping lifecycle in `OsMemory` and `Mapping`.
- `Mapping` construction is private to `os.rs`; every `Mapping` must come from `OsMemory::map` so its `(base, len)` always describes a live, uniquely-owned mmap region (`NonZeroUsize` length, page-aligned base, page-multiple len). Do not widen this constructor's visibility as a shortcut.
- `OsMemory` has no raw `unmap` escape hatch. Anything that owns mmap'd storage must hold a `Mapping` (or a type that owns one) and rely on `Drop` to release it; do not reintroduce `mem::forget`-plus-manual-`munmap` patterns.
- `AddressRange` is ownership-free `(base, len)` geometry (e.g. extent user sub-ranges). Do not put munmap or uniqueness on it.
- Keep pointer lookup and ownership publication in `PageMap`.
- `L1Table` is a two-array root: hot `tables: [AtomicPtr<L2Table>; L1_ENTRIES]` for lock-free `get`, cold `meta: [L1Meta; L1_ENTRIES]` for per-L2 write exclusion + mmap ownership. Do not re-merge cold fields into the get-indexed slots.
- `PageMap` install is once-only via `AtomicPtr` CAS (L1 root and each L2 tip in `tables`). Page stamps use **per-L2** write exclusion on `L1Meta::write` (`AtomicBool`, zero-filled ⇒ unlocked) plus `AtomicMapEntry::store`. Do not reintroduce a global PageMap ownership/stamp mutex, occupancy counters, or per-page CAS as the stamp protocol.
- L1 mmap ownership lives in `PageMap::l1_mapping: UnsafeCell<Option<Mapping>>`; each L2 mmap lives in `L1Meta::mapping`. Install protocol: mmap → CAS-publish table pointer in `tables` → only the CAS winner stores `Mapping` in the paired meta cell; loser drops its `Mapping`. Mapping cells are written once by the winner and read only on exclusive `PageMap` drop — `get` never touches meta or mapping cells.
- L2 tables are retained for the `PageMap` lifetime (no empty-L2 reclaim in v0.5).
- Multi-page insert/remove: install L2s (insert), take `L1WriteGuard` over touched L1 metas in ascending L1 order (Drop unlocks), validate under the guard, store. Failed validate writes nothing; Drop still unlocks. Do not pair manual unlock at call sites.
- Zero-fill (anonymous mmap) must stay a valid empty state: null `tables` slots, unlocked `L1Meta::write`, `Option<Mapping>` niche `None`. Prove niches with unit tests when changing the layout.
- `get` stays lock-free: Acquire loads on L1 tip → `tables[l1]` → page entry only; never takes `write` or reads `meta`. Compose as `PageMap::get` → `L1Table::owner` → `L2Table::owner`.
- `PageMap::{publish_run,publish_extent,unpublish_extent}` take `&Mapping` (not a raw `AddressRange`). Preserve `PageOwner` pointer lifetime assumptions: owners stay live until their page-map range is removed.
- `PageMap` has exactly one in-memory representation per published range: every page in a run or extent range gets a direct per-page entry. Do not add a second encoding (e.g. a span/run-length record) that `PageMap` silently falls back to when the primary one is exhausted or unavailable; if a future optimization needs a denser encoding, replace the representation everywhere rather than layering a fallback next to it.
- Extents use `publish_extent` / `unpublish_extent`. Runs only `publish_run` today: empty-run reclaim is not implemented, so do not add a dead `unpublish_run` (or `#[allow(dead_code)]`) ahead of that work. When reclaim lands, add `unpublish_run` with its first real caller in the same change.
- `PageOwner` stays a concrete `Run`/`Extent` enum rather than an opaque `NonNull` + kind tag: callers in `allocator.rs`, `heap/run/heap.rs`, and `heap/extent/heap.rs` pattern-match on it and immediately dereference the typed pointer, so erasing the type would only add casts at every call site without removing any duplication. `MapEntry` already carries the kind bit needed for compact storage; revisit `PageOwner` only if a caller needs to hold page-map results without knowing the arena type.
- Put page-map behavior on the owning type: `AtomicMapEntry` (load/store), `L2Table` (owner + segment match/write under caller-held L2 write lock), `L1Meta` (write exclusion + mapping ownership), `L1Table` / `L1WriteGuard` (dense tips, install, ordered range lock + stamp), `PageMap` (publish/get composition).
- `L1Table` is sized for the full 48-bit address space and relies on the OS to lazily back it with physical pages; it is not densified in v0.5 beyond the hot/cold array split. Revisit VA shape only with a profile showing L1 reservation or first-touch cost matters.
- Add concurrent publish smoke tests (disjoint ranges, same-L2 disjoint pages, overlapping race) when changing stamp exclusion or install protocol.
- Keep unsafe pointer/provenance code narrow and adjacent to safety comments.
- Add page-map tests for overlap, removal, and L2 boundary behavior when changing lookup logic.
