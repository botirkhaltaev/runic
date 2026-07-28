# AGENTS.md

## Goals

- Performance is the top priority on hot paths — but **data-driven only**: profile before and after (`scripts/profile.sh`); never infer micro-opts, inlining, or layout “wins” without measurements.
- Clean, idiomatic, readable Rust. No hacks at code or architecture level (no clever dual paths, kludges, or “temporary” shims that become permanent).
- Safe Rust first; `unsafe` only for OS/ownership contracts or **measured** hot paths (narrow + SAFETY).
- Explicit ownership entities, fail-closed frees, auditable invariants — not line-for-line ports.
- Composable APIs: behavior on the owning entity; no one-caller shims, pass-throughs, dual APIs, or `*_v2` / `*_slow` / `*_miss` / `*_nonlocal` names (`#[cold]` only).

## Conventions

- Prefer `NonZero*` / `NonNull` / named fields. No useless helpers — especially free (module-level) one-liners / cast wrappers / pass-throughs. Put behavior on the owning type; helpers only for real reuse, a clearer ownership boundary, or a **profiled** cold-path factor.
- **One handle** — never the same object as both `NonNull<T>` and `&T`. Project fields once at the boundary; do not thread `&AllocatorInner` with `&PageMap` / `&Heaps`.
- TLS hot paths: `ThreadHeap::{alloc,alloc_extent,free_run,free_extent,lookup}` take `NonNull<AllocatorInner>` (identity) + `&PageMap` projected once at `Allocator`. Cold unbound: `Allocator::{bind_alloc,free_remote}`.
- Naming: short, clear, domain words only — same term means the same thing everywhere. No long compound jargon, invented synonyms, or parallel names for one concept. Frontend `alloc`, domain block/extent `allocate`, checkout `acquire`. Free protocol: `free` / `claim` / `accept`. Prefer existing vocabulary (`run`, `extent`, `heap`, `inbox`, `flush`, `bind`) over new coinages.
- Indices: `Arena` / `HeapId` / `RunId` / `ExtentId` use `u32`; convert to `usize` only when indexing Rust arrays or doing pointer/byte math — no free cast-wrapper helpers.
- Remote free: claim → `Heap::enqueue` (Active; lease before new `try_queue`) or `Heaps::lock` → `LockedHeap` → owner `flush` → `accept`. Coalesce by owner (`Inbox`), never a freer TLS batch.
- Flush policy: sticky empty = local/OS acquire then flush (`refill`); unbound = `alloc_after_bind` / `alloc_extent_after_bind` (flush then alloc); sticky free hit = `Run::free` only (no available relink).
- `Layout` only at the public boundary → `LayoutSpec` inward once.
- No root/shared ownership heap; every run/extent has `HeapId`. Capabilities: shared `&Heap` = atomics only (`enqueue` / mode); Active body = `ThreadHeap` only; Draining body + reclaim = `LockedHeap` only (`Heaps::lock`). No `Heap::state()` projection; no `*_fresh` dual alloc APIs.
- One abort sink: `Allocator::abort`. Preserve abort kinds through `HeapError` (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`). Never hold the heaps arena mutex across flush / accept / user-memory copies.
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

## v0.5 Scope

- In: Linux x86_64, Rust stable, `GlobalAlloc`, owner-local heaps, run/extent retention, remote-free, `realloc` / `alloc_zeroed`, tests, benches.
- Out: per-CPU/RSEQ, quarantine, canaries, hugepages, NUMA, C ABI, ML placement, dashboards, background purge.
