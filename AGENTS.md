# AGENTS.md

## Goals

- Performance is the top priority on hot paths — but **data-driven only**: profile before and after (`scripts/profile.sh`); never infer micro-opts, inlining, or layout “wins” without measurements.
- Clean, idiomatic, readable Rust. No hacks at code or architecture level (no clever dual paths, kludges, or “temporary” shims that become permanent).
- Safe Rust first; `unsafe` only for OS/ownership contracts or **measured** hot paths (narrow + SAFETY).
- Explicit ownership entities, fail-closed remote admission and interior/foreign pointers, auditable invariants — not line-for-line ports. Owner double-free is undefined.
- Composable APIs: behavior on the owning entity; no one-caller shims, pass-throughs, dual APIs, or `*_v2` / `*_nonlocal` names. `#[inline(never)]` outlines only (`alloc_miss` / `dealloc_slow` / `push_available`); `#[cold]` is abort / bind / map / remote / unbind / discard / adopt.

## Conventions

- Prefer `NonZero*` / `NonNull` / named fields. No useless helpers — especially free (module-level) one-liners / cast wrappers / pass-throughs. Put behavior on the owning type; helpers only for real reuse, a clearer ownership boundary, or a **profiled** cold-path factor.
- **One handle** — never the same object as both `NonNull<T>` and `&T`. Project fields once at the boundary; `Allocator::ctx()` is `AllocatorCtx` — do not thread `&PageMap` / `&Heaps` separately.
- Small hit: `ThreadHeaps::{alloc,free}` take no inner. `&PageMap` on miss. Cold unbound: `Allocator::{bind_alloc,free_remote}`.
- Naming: short, clear, domain words only — same term means the same thing everywhere. No long compound jargon, invented synonyms, or parallel names for one concept. Frontend `alloc`, domain block/extent `allocate`, checkout `acquire`, current-run `extend`. Free protocol: `free` / `claim` / `accept`. Prefer existing vocabulary (`run`, `extent`, `heap`, `inbox`, `flush`, `bind`, `current`, `extend`) over new coinages.
- Indices: `Arena` / `HeapId` / `RunId` / `ExtentId` use `u32`; convert to `usize` only when indexing Rust arrays or doing pointer/byte math — no free cast-wrapper helpers.
- Remote free: claim → `adopt` a Draining heap (then owner `free`) or `Heap::enqueue` (Active; lease before new `try_queue`) or `Heaps::{free,flush}` (Draining). Coalesce by owner (`Inbox`), never a freer TLS batch.
- Flush policy: current-run empty = `extend`; inbox flush if nonempty; then local/OS `acquire_run`. Unbound = `bind` then `flush` then alloc; hit = current pop / `Run::free` (ignore `RunFree` / Discard). `push_available` is miss / slow / unbind. Inbox `flush` is remote `accept` only. `lookup` is miss / realloc.
- `Layout` only at the public boundary → `LayoutSpec` inward once.
- No root/shared ownership heap; every run/extent stores its owning `&Heap` and derives `HeapId`. Shared `&Heap` = atomics only (`id` / `active_id` / `enqueue` / mode / live counts). TLS keeps the generation token captured on bind/adopt so unbind cannot close a later incarnation. `PageOwner` carries process-lifetime run headers / immortal extent slots; extent unmap drops only `Mapping`. Active exclusive = `ThreadHeaps` + `require_inner` + `AllocatorCtx`. Draining = `Heaps::{free,flush}` + `AllocatorCtx`. No `Heap::state()` projection; no `*_fresh` dual alloc APIs.
- One abort sink: `Allocator::abort`. Preserve abort kinds through `HeapError` (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`). `HeapError::DoubleFree` is remote `claim` / interior-foreign only — not owner DF. Never hold the arena grow lock across flush / accept / user-memory copies.
- No allocator-internal `Vec` / `Box` / `HashMap` / `String` / formatting / panic unless recursion risk is addressed.
- `#![deny(unsafe_op_in_unsafe_fn)]`. No test-only methods on production `impl` blocks.
- No backward compatibility for public or internal APIs — reshape in place; delete dual paths, aliases, and parallel old names. Best architecture and code always win.
- Nested `AGENTS.md`: subtree rules only; closest wins; shorter than root; no root duplication; <60 lines (cap 100). Update the matching `README.md` when APIs change.

## Commands

| Task | Command |
|------|---------|
| Check | `cargo check --workspace` |
| Test | `cargo test --workspace` |
| Test crate | `cargo test -p <crate>` |
| Format | `cargo fmt --all` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Bench build | `cargo bench -p runic-bench --no-run` |
| Profile | `scripts/profile.sh` |
| Preload `.so` | `cargo build -p runic-cabi --release` |
| Publish | `cargo publish --workspace` |

## External References

| Need | File |
|------|------|
| Thesis, milestones | `ROADMAP.md` |
| Architecture | `ARCHITECTURE.md` |
| Compatibility | `COMPATIBILITY.md` |
| Measurement log | `diary.md` |
| librseq-in-Rust word ops (not a runic hit) | https://github.com/botirkhaltaev/rseq-rs |
| Install / usage | `README.md` |
| Core crate | `crates/runic-core/README.md` |
| Public `GlobalAlloc` crate | `crates/runic/README.md` |
| C malloc-family / LD_PRELOAD | `crates/runic-cabi/README.md` |
| Inspiration only (do not copy code) | `allocator-refs/` |

## Scope

- v0.8 in: Linux x86_64, Rust nightly, `#[thread_local]` `THREAD_HEAPS`, `GlobalAlloc`, C malloc-family LD_PRELOAD (`runic-cabi`), owner-local heaps, two equal TLS heaps, TLS current run, immortal extent slots, `Heap` live atomics, lock-free `Heaps::get`, draining `admit`/`flush` with optional `owner`, run/extent retention, remote-free, `realloc` / `alloc_zeroed`, tests, real-workload benches.
- v0.9 in: `Memory` trait behind the `Os` alias (`Linux` impl owns `libc`), payload hugepage Off/Thp and NUMA Off/Local, `RunicAlloc::new().with_*` (no builder), cabi `Allocator::preload` + `RUNIC_*`, Fast only (Safe/Hardened abort at init). `Hints` default Off/Off after the Fast screen.
- v0.9 out: quarantine, canaries, Safe owner-DF, Hardened, reclaim, mallinfo, fork, extra OS, `MAP_HUGETLB` / `MAP_HUGE_1GB`. Production sequence: `ROADMAP.md` 0.10–0.14.
- Next: `ROADMAP.md` 0.10 Safe. Hit free is `Run::free` (`__rust_dealloc` has no callee-saved). C `free` recovers the owner via `PageMap` (`header_of` is not safe on extents); `free(NULL)` is a C no-op. `header_of` checks raw `base` before constructing `Run`. `issued` / `link` / claims live on `RemoteLine`. Live counts are `Heap` atomics; reclaim scans after. Zeroed Keep reuse ≥64 KiB discards pages without the Discard-insert clean flag; below that, memset. `ExtentPolicy::Discard` matches snmalloc — not a medium class. `Heaps::get` is a lock-free `Arena` read. Two equal TLS heaps; a third adopt stays on `Heaps::free` (lost on `channel_pipeline`). Do not compact `CLASS_FOR_SIZE`, retry first-fit extent reuse, identity, batch take, O(1) TLS steal, `#135` RSEQ, per-CPU heaps on rseq-rs, locate-offset dual free, a third TLS slot, reclaim live-scan elimination, realloc known-owner reuse, or the `spawn_churn` fault package. Do not port snmalloc. Claimed remote frees retry Active/Draining transitions; a generation advance proves the owner accepted the claim.

## Learned User Preferences

- Keep Criterion benches as real workloads: one file per workload, simple layout, no adhoc scripts or synthetic-only suites. Tests stay Cargo-native: no extra fixture binaries or polling waits when `cargo test` suffices.
- Never merge dead code or `#[allow(clippy)]`; fix the lint instead.
- Prefer isolating `unsafe` in leaf entities so it can be tested; do not leave it on higher layers when a leaf boundary works.
- Tests must exercise production types: no test-only structs, entities, or helpers that shadow real owners. Generic type-parameter stubs (`TestNode` for `Inbox<T>`, `DropCounter`/`Large` for `Arena<T>`) are OK.
- Do not invent fake entities; model real ownership.
- Prefer `const` constructors and `static`/`OnceLock` for test fixtures over `Box::leak`.
- Do not shadow or add redundant reassignments (e.g. `let start = bump`).
- Compare owners with entity methods/traits (`owns`), not `ptr::eq` or other raw pointer ops.
- TLS `ThreadHeap` slots are equal; scan all of them (`idle`) — do not special-case a primary/first heap.
- Do not keep unused parameters; drop redundant args when a domain type already carries the info (e.g. `SizeClass` vs inward `Layout`).
- Human-facing writing: no em dashes and no signs of AI usage.
- Name the malloc-family crate and TLS feature `c-abi`, not `preload`.

## Learned Workspace Facts

- `ROADMAP.md` is thesis and milestones only; architecture is `ARCHITECTURE.md`; compatibility is `COMPATIBILITY.md`; profiling notes and tried experiments go in `diary.md`.
- RSEQ experiments use the separate `rseq-rs` crate, not an in-tree rseq implementation.
- C malloc-family LD_PRELOAD is the published `runic-cabi` crate (cdylib `librunic.so`), not a `runic-alloc` feature. `runic-core`'s `c-abi` feature is pthread TLS for thread-exit under preload; default is `std::thread_local!`.
- Publish with `cargo publish --workspace`; do not wait-loop on crates.io.
- Default branch is `master`, not `main`.
- Config lives on the allocator: `RunicAlloc::new().with_x()` / `with_mode`, no separate builder type. Do not invent crate README samples for unshipped `with_mode`.
