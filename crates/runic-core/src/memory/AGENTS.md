# AGENTS.md

Scope: `crates/runic-core/src/memory/`.

- Every `Mapping` comes from a `Memory` method on the `Os` alias; do not name `Linux` outside `os.rs` / `mod.rs`, and keep OS calls in `os.rs`. `Mapping` Drop owns the kept region's `munmap`; only aligned-map construction may trim its temporary over-map. `discard` is `madvise(MADV_DONTNEED)` and returns whether it succeeded. Payload maps use `map_payload` / `map_aligned_payload` (`Mapping::prefer` after the map); process/arena/page-map maps do not.
- `PageMap::get` is lock-free via hot `tables` only; never touches cold `writes` / `mappings`. Small free probes `Run::header_of`; this map is miss / extent / fallback.
- Checked `Page::split` on untrusted pointers (fail closed outside 48-bit geometry).
- One in-memory encoding per published range — no layered span fallback beside per-page stamps.
- `publish` / `unpublish` take a `PageOwner` and derive its pages; never pass geometry alongside an owner. Runs stay published until empty-run reclaim has a real caller.
- Details: `crates/runic-core/src/memory/README.md`.
