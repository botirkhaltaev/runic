# AGENTS.md

## Goals

- Performance is the top priority on hot paths — but **data-driven only**: profile before and after (`scripts/profile.sh`); never infer micro-opts, inlining, or layout “wins” without measurements.
- Clean, idiomatic, readable Rust. No hacks at code or architecture level (no clever dual paths, kludges, or “temporary” shims that become permanent).
- Safe Rust first; `unsafe` only for OS/ownership contracts or **measured** hot paths (narrow + SAFETY).
- Explicit ownership entities, fail-closed remote admission and interior/foreign pointers, auditable invariants — not line-for-line ports. Owner double-free is undefined.
- Composable APIs: behavior on the owning entity; no one-caller shims, pass-throughs, dual APIs, or `*_v2` / `*_nonlocal` names. `#[inline(never)]` outlines only (`alloc_miss` / `dealloc_slow` / `push_available`); `#[cold]` is abort / bind / map / remote / unbind.

## Conventions

- Prefer `NonZero*` / `NonNull` / named fields. No useless helpers — especially free (module-level) one-liners / cast wrappers / pass-throughs. Put behavior on the owning type; helpers only for real reuse, a clearer ownership boundary, or a **profiled** cold-path factor.
- **One handle** — never the same object as both `NonNull<T>` and `&T`. Project fields once at the boundary; do not thread `&AllocatorInner` with `&PageMap` / `&Heaps`.
- Small hit: `ThreadHeap::{alloc,cached_run}` take `*mut AllocatorInner` (`matches` is pointer equality; null fails it). `&PageMap` on miss. Cold unbound: `Allocator::{bind_alloc,free_remote}`.
- Naming: short, clear, domain words only — same term means the same thing everywhere. No long compound jargon, invented synonyms, or parallel names for one concept. Frontend `alloc`, domain block/extent `allocate`, checkout `acquire`, current-run `extend`. Free protocol: `free` / `claim` / `accept`. Prefer existing vocabulary (`run`, `extent`, `heap`, `inbox`, `flush`, `bind`, `current`, `extend`) over new coinages.
- Indices: `Arena` / `HeapId` / `RunId` / `ExtentId` use `u32`; convert to `usize` only when indexing Rust arrays or doing pointer/byte math — no free cast-wrapper helpers.
- Remote free: claim → `Heap::enqueue` (Active; lease before new `try_queue`) or `Heaps::{enqueue,free,flush}` (Draining). Coalesce by owner (`Inbox`), never a freer TLS batch.
- Flush policy: current-run empty = `extend`; inbox flush if nonempty; then local/OS `acquire_run`. Unbound = `alloc_after_bind` / `alloc_extent_after_bind` (flush then alloc); hit = current pop / page-cache `Run::free` (`push_available` only on `was_full`). Inbox `flush` is remote `accept` only.
- `Layout` only at the public boundary → `LayoutSpec` inward once.
- No root/shared ownership heap; every run/extent has `HeapId`. Capabilities: shared `&Heap` = atomics only (`enqueue` / mode); Active exclusive = `ThreadHeap` + `try_inner` + `HeapCtx { pages }`; Draining = `Heaps::{enqueue,free,flush}` + `HeapsCtx`. No `Heap::state()` projection; no `*_fresh` dual alloc APIs.
- One abort sink: `Allocator::abort`. Preserve abort kinds through `HeapError` (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`). `HeapError::DoubleFree` is remote `claim` / interior-foreign only — not owner DF. Never hold the heaps arena mutex across flush / accept / user-memory copies.
- No allocator-internal `Vec` / `Box` / `HashMap` / `String` / formatting / panic unless recursion risk is addressed.
- `#![deny(unsafe_op_in_unsafe_fn)]`. No test-only methods on production `impl` blocks.
- No backward compatibility for public or internal APIs — reshape in place; delete dual paths, aliases, and parallel old names. Best architecture and code always win.
- Nested `AGENTS.md`: subtree rules only; closest wins; shorter than root; no root duplication; <60 lines (cap 100). Update the matching `README.md` when APIs change. Skill: `.agents/skills/agents-md`.

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

## External References

| Need | File |
|------|------|
| Thesis, milestones, architecture | `ROADMAP.md` |
| Install / usage | `README.md` |
| Core crate | `crates/runic-core/README.md` |
| Public `GlobalAlloc` crate | `crates/runic/README.md` |
| Inspiration only (do not copy code) | `allocator-refs/` |

## Scope

- v0.6 in: Linux x86_64, Rust stable, `GlobalAlloc`, owner-local heaps, TLS current run, run/extent retention, remote-free, `realloc` / `alloc_zeroed`, tests, benches.
- v0.6 out: quarantine, canaries, hugepages, NUMA, C ABI, ML placement, dashboards, background purge.
- Next: leftover vs snmalloc is still hit instruction count after the locate diet. Do not retry identity, batch take, O(1) TLS steal, or `#135` RSEQ on single-thread churn. Do not port snmalloc.
