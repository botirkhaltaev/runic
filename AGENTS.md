# AGENTS.md

## Goals

- Performance is the top design priority on hot paths (simple, direct, minimal work).
- Prefer safe idiomatic Rust; use `unsafe` only where needed for OS/ownership contracts or measured hot-path performance (narrow blocks + SAFETY comments).
- Best-practice allocator design: explicit ownership entities, fail-closed frees, auditable invariants — not line-for-line ports.
- General composable clean APIs; no one-caller overfit methods, shims, or `*_slow` / `*_miss` / `*_nonlocal` names (`#[cold]` only).

## Conventions

- Put behavior on the owning entity; prefer `NonZero*` / `NonNull` / named fields; avoid free one-line helpers and pass-throughs.
- Owner-local TLS vs remote table: `ThreadHeap::{alloc,alloc_extent,free,free_extent}` / `Allocator::{alloc_remote,alloc_extent_remote,free_remote}`; domain ops `free` / `claim` / `accept`.
- One remote-free protocol: claim → batch → `HeapTable::publish` → flush/`accept` (including `Draining` late frees).
- `Layout` only at the public boundary; convert once to `LayoutSpec` and pass it inward (`SizeClasses::id_for`, extents, resize).
- No shared/root ownership heap; every run/extent is stamped with `HeapId`.
- Exactly one abort sink: `Allocator::abort`. Never hold `Mutex<HeapTable>` across a user-memory copy.
- No allocator-internal `Vec` / `Box` / `HashMap` / `String` / formatting / panic unless recursion risk is addressed.
- `#![deny(unsafe_op_in_unsafe_fn)]`. No test-only methods on production `impl` blocks.
- When editing any `AGENTS.md`, follow `.agents/skills/agents-md`: nested files only for subtree-specific rules; closest wins; shorter than root; no root duplication; target <60 lines (hard cap 100). Revamp/clean the nearest file when APIs change; update the matching `README.md`.

## Commands

| Task | Command |
|------|---------|
| Check | `cargo check --workspace` |
| Test | `cargo test --workspace` |
| Test crate | `cargo test -p <crate>` |
| Format | `cargo fmt --all` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Bench build | `cargo bench -p runic-bench --no-run` |

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
