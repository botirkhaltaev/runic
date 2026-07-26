# AGENTS.md

## Goals

- Correct ownership and fail-closed frees first; within that, maximize performance.
- Safe idiomatic Rust by default; `unsafe` only for OS/ownership boundaries or profiled hot paths (narrow blocks + SAFETY comments).
- General composable entity APIs — no one-caller overfit methods, shims, or `*_slow` / `*_miss` / `*_nonlocal` names (`#[cold]` only).

## Conventions

- Put behavior on the owning entity; prefer `NonZero*` / `NonNull` / named fields; avoid free one-line helpers and pass-throughs.
- Owner-local TLS vs remote table: `ThreadHeap::{alloc,alloc_extent,free,free_extent}` / `Allocator::{alloc_remote,alloc_extent_remote,free_remote}`; domain ops `free` / `claim` / `accept`.
- One remote-free protocol: claim → batch → `HeapTable::publish` → flush/`accept` (including `Draining` late frees).
- `Layout` only at the public boundary; convert once to `LayoutSpec` and pass it inward (`SizeClasses::id_for`, extents, resize).
- No shared/root ownership heap; every run/extent is stamped with `HeapId`.
- Exactly one abort sink: `Allocator::abort`. Never hold `Mutex<HeapTable>` across a user-memory copy.
- No allocator-internal `Vec` / `Box` / `HashMap` / `String` / formatting / panic unless recursion risk is addressed.
- `#![deny(unsafe_op_in_unsafe_fn)]`. No test-only methods on production `impl` blocks.
- Nested `AGENTS.md` only for subtree-specific rules; closest file wins; keep nested files shorter than root; do not repeat root. Target <60 lines (hard cap 100). Revamp the nearest file when APIs change; update the matching `README.md`.

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
