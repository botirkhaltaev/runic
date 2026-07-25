# AGENTS.md

Scope: `crates/runic-core/src/memory/`.

- Keep OS mapping lifecycle in `OsMemory` and `Mapping`.
- `Mapping` construction is private to `os.rs`; every `Mapping` must come from `OsMemory::map` so its `(base, len)` always describes a live, uniquely-owned mmap region (`NonZeroUsize` length, page-aligned base, page-multiple len). Do not widen this constructor's visibility as a shortcut.
- `OsMemory` has no raw `unmap` escape hatch. Anything that owns mmap'd storage must hold a `Mapping` (or a type that owns one) and rely on `Drop` to release it; do not reintroduce `mem::forget`-plus-manual-`munmap` patterns.
- `AddressRange` is ownership-free `(base, len)` geometry (e.g. extent user sub-ranges). Do not put munmap or uniqueness on it.
- Keep pointer lookup and ownership publication in `PageMap`.
- `L1Table` hot root is `slots: [L1Slot; L1_ENTRIES]` where each `L1Slot` is tip (`AtomicPtr<L2Table>`) + per-L2 `write` (`AtomicBool`) — 16-byte stride. Do not store `Mapping` in the get/stamp-indexed hot slots.
- L2 mmap ownership is sparse: `PageMap::l2_mappings: Mutex<Arena<Mapping>>`, touched only on L2 install CAS win and `PageMap` drop. Do not reintroduce a full-size L1 Mapping sideband array (dual first-touch on publish).
- `L2Table` is page stamps only and must stay exactly eight pages (`size_of::<L2Table>() == 0x8000`). Do not put write exclusion on `L2Table` if that rounds the mmap past `0x8000`.
- `PageMap` install is once-only via `AtomicPtr` CAS (L1 root and each L2 tip). Page stamps use **per-L2** write exclusion on `L1Slot::write` (zero-filled ⇒ unlocked) plus `AtomicMapEntry::store`. Do not reintroduce a global PageMap stamp mutex, occupancy counters, or per-page CAS as the stamp protocol. The `l2_mappings` mutex is install/drop registry only — never taken by `get` or stamp.
- L1 mmap ownership lives in `PageMap::l1_mapping`. Install protocol: mmap L2 → claim registry slot → CAS-publish tip → winner `Arena::insert`s `Mapping`; loser releases claim and drops `Mapping`.
- L2 tables are retained for the `PageMap` lifetime (no empty-L2 reclaim in v0.5).
- Multi-page insert/remove: install L2s (insert), take `L1WriteGuard` over touched L1 slots in ascending L1 order (Drop unlocks), validate under the guard, store. Failed validate writes nothing; Drop still unlocks. Do not pair manual unlock at call sites.
- Zero-fill (anonymous mmap) must stay a valid empty state: null tips, unlocked `write`, empty L2 page entries. Prove niches with unit tests when changing the layout.
- `get` stays lock-free: Acquire loads on L1 tip → `L1Slot::table` → page entry only; never takes `write` or the `l2_mappings` mutex. Compose as `PageMap::get` → `L1Table::owner` → `L2Table::owner`.
- `PageMap::{publish_run,publish_extent,unpublish_extent}` take `&Mapping` (not a raw `AddressRange`). Preserve `PageOwner` pointer lifetime assumptions: owners stay live until their page-map range is removed.
- `PageMap` has exactly one in-memory representation per published range: every page in a run or extent range gets a direct per-page entry. Do not add a second encoding (e.g. a span/run-length record) that `PageMap` silently falls back to when the primary one is exhausted or unavailable; if a future optimization needs a denser encoding, replace the representation everywhere rather than layering a fallback next to it.
- Extents use `publish_extent` / `unpublish_extent`. Runs only `publish_run` today: empty-run reclaim is not implemented, so do not add a dead `unpublish_run` (or `#[allow(dead_code)]`) ahead of that work. When reclaim lands, add `unpublish_run` with its first real caller in the same change.
- `PageOwner` stays a concrete `Run`/`Extent` enum rather than an opaque `NonNull` + kind tag: callers in `allocator.rs`, `heap/run/heap.rs`, and `heap/extent/heap.rs` pattern-match on it and immediately dereference the typed pointer, so erasing the type would only add casts at every call site without removing any duplication. `MapEntry` already carries the kind bit needed for compact storage; revisit `PageOwner` only if a caller needs to hold page-map results without knowing the arena type.
- Put page-map behavior on the owning type: `AtomicMapEntry` (load/store), `L2Table` (owner + segment match/write under caller-held write lock), `L1Slot` (tip + write exclusion), `L1Table` / `L1WriteGuard` (hot slots, ordered range lock + stamp), `PageMap` (publish/get composition + sparse L2 mapping registry).
- `L1Table` is sized for the full 48-bit address space and relies on the OS to lazily back it with physical pages; it is not densified in v0.5 beyond 16-byte hot slots. Revisit VA shape only with a profile showing L1 reservation or first-touch cost matters.
- Add concurrent publish smoke tests (disjoint ranges, same-L2 disjoint pages, overlapping race) when changing stamp exclusion or install protocol.
- Keep unsafe pointer/provenance code narrow and adjacent to safety comments.
- Add page-map tests for overlap, removal, and L2 boundary behavior when changing lookup logic.
