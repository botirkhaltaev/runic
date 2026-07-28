# AGENTS.md

## Goals

- Hot-path performance first: simple, direct, minimal work.
- Safe idiomatic Rust; `unsafe` only for OS/ownership contracts or measured hot paths (narrow + SAFETY).
- Explicit ownership entities, fail-closed frees, auditable invariants — not line-for-line ports.
- Composable APIs: behavior on the owning entity; no one-caller shims, pass-throughs, dual APIs, or `*_v2` / `*_slow` / `*_miss` / `*_nonlocal` names (`#[cold]` only).

## Conventions

- Prefer `NonZero*` / `NonNull` / named fields; no free one-line helpers.
- **One handle** — never the same object as both `NonNull<T>` and `&T`. Project fields once at the boundary; do not thread `&AllocatorInner` with `&PageMap` / `&HeapDirectory`.
- TLS hot paths: `ThreadHeap::{alloc,alloc_extent,free_run,free_extent,lookup_owner}` take `NonNull<AllocatorInner>` (identity) + `&PageMap` projected once at `Allocator`. Cold unbound: `Allocator::{alloc_unbound,free_cross_heap}`.
- Naming: frontend `alloc`, domain block/extent `allocate`, checkout `acquire`. Domain free protocol: `free` / `claim` / `accept`.
- Remote free: claim → `HeapSlot::enqueue` (Active; lease only if newly queued) or `HeapDirectory::lock` → `LockedSlot` → owner `flush` → `accept`. Coalesce by owner (`Inbox`), never a freer TLS batch.
- Flush policy: sticky empty = local/OS acquire then flush (`refill_sticky`); unbound = flush then alloc; sticky free hit = `Run::free` only (no available relink).
- `Layout` only at the public boundary → `LayoutSpec` inward once.
- No root/shared ownership heap; every run/extent has `HeapId`. `HeapSlot` owns metadata via private `SlotHeap` (no thin public `Heap`).
- One abort sink: `Allocator::abort`. Preserve abort kinds through `HeapError` (`InvalidRunPointer` / `InvalidExtentPointer` / `MissingExtent`). Never hold the directory lifecycle mutex across a user-memory copy.
- No allocator-internal `Vec` / `Box` / `HashMap` / `String` / formatting / panic unless recursion risk is addressed.
- `#![deny(unsafe_op_in_unsafe_fn)]`. No test-only methods on production `impl` blocks.
- No backward-compat or parallel old paths when reshaping APIs.
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
