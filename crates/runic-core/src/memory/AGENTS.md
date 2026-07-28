# AGENTS.md

Scope: `crates/runic-core/src/memory/`.

- Every `Mapping` from `OsMemory::map` only; no raw `unmap` / `mem::forget`+munmap.
- `PageMap::get` lock-free via hot `tables` only; never touches cold `writes` / `mappings`.
- Checked `Page::split` on untrusted pointers (fail closed outside 48-bit geometry).
- One in-memory encoding per published range — no layered span fallback beside per-page stamps.
- No dead `unpublish_run` until empty-run reclaim has a real caller.
- Details: `crates/runic-core/src/memory/README.md`.
