# AGENTS.md

## Project Context

Ferralloc is a Rust-native allocator project. The goal is not to port an existing allocator line-for-line, but to build a correct Rust allocator core with explicit, auditable invariants.

Current milestone: a global-lock allocator that can run real Rust programs and survive randomized allocation traces.

Use `ROADMAP.md` as the source of truth for project thesis, current scope, architecture, testing direction, and later milestones.

## User Preferences

- Prefer entity-based architecture and design.
- Avoid free helper functions unless there is a strong reason.
- Put behavior on the type that owns the data or invariant.
- No backward compatibility is needed for internal allocator APIs unless explicitly requested.
- Prefer best-practice, idiomatic Rust over preserving existing internal shapes.
- Prefer general, composable APIs over narrow methods tailored to one caller.
- Keep naming simple, direct, and domain-based.
- Avoid overfit method names that encode current implementation details or a single call path.
- Model real domain concepts as explicit entity types when they own data, invariants, or behavior.
- Apply architectural feedback repository-wide, not only at the site where an issue was noticed.
- Prefer small cohesive types with clear responsibilities over broad manager APIs.
- API shape should make invalid states hard to express without adding unnecessary abstraction.
- Keep code and architecture simple wherever possible.
- Write idiomatic, performance-aware Rust without premature optimization.
- Introduce abstractions only when they reduce complexity or clarify invariants.
- Avoid callback-style helper patterns for ordinary control flow; prefer direct, explicit calls.
- Avoid lint workarounds that reduce code quality; remove or narrow lints that fight idiomatic Rust.
- Do not use `#[allow]` or `#[expect]` as a shortcut when a cleaner design or refactor is reasonable.

## v0.1 Scope

Build only: Linux x86_64, Rust stable, `GlobalAlloc`, global lock, mmap-backed runs for small size classes, mmap-backed extents for dedicated allocations, out-of-line metadata, page-indexed pointer lookup, run block-boundary checks, extent exact-pointer checks, basic `realloc`, basic `alloc_zeroed`, and randomized tests.

Do not add profiles, thread-local heaps, remote frees, quarantine, canaries, hugepages, NUMA, C ABI support, ML placement, or dashboards yet.

## Core Invariant

Every returned pointer maps to exactly one page-map entry. Runs own one mapping and divide it into fixed-size reusable blocks from one size class. Extents own one mapping dedicated to exactly one returned allocation. Every free must map back to a known entry: run frees must be valid block boundaries, and extent frees must be the exact returned pointer.

Correctness comes before speed.

## Architecture

Use this first:

```text
GlobalAlloc
  -> Allocator
      -> Heap
      -> PageMap
      -> RunTable
      -> ExtentTable
      -> Run
          -> FreeList
      -> Extent
      -> OsMemory
```

Use one global lock around `Heap`.

## Rust Rules

- Use `#![deny(unsafe_op_in_unsafe_fn)]`.
- Keep unsafe code small, explicit, and local.
- Prefer methods on `Allocator`, `Heap`, `Run`, `Extent`, `PageMap`, `RunTable`, `ExtentTable`, `FreeList`, `OsMemory`, and `SizeClasses`.
- Avoid allocator-internal `Vec`, `Box`, `HashMap`, `String`, formatting, or panic paths unless recursion risk is addressed.
- Abort on invalid frees in v0.1.
- Do not unwind across allocator boundaries.

## Allocator References

`allocator-refs/` contains external allocator projects and benchmark suites for inspiration. Treat it as read-only reference material. Use it for test ideas, invariants, workload shapes, and benchmark categories, not copied implementation code.

## Issue Tracking

- If an agent notices a real issue, critique, or improvement that is out of scope for the current task, create or update a GitHub issue instead of expanding scope.
- Current known follow-ups: improve `PageMap` metadata allocation, add block-state tracking for double-free detection, and revisit `RunTable` test/production capacity differences.

## Commands

- Check: `cargo check --workspace`
- Test: `cargo test --workspace`
- Format: `cargo fmt --all`
- Lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
