# AGENTS.md

## Project Truth

- Runic is a Rust-native allocator project; build a correct allocator core with explicit, auditable invariants rather than porting another allocator line-for-line.
- Current milestone: a global-lock allocator with optimized single-thread small allocation paths, stable run/extent metadata ownership, and randomized trace coverage.
- Use `ROADMAP.md` as the source of truth for thesis, scope, architecture, testing direction, and later milestones.
- Treat this file as a non-deterministic test suite for agents: each bullet is an assertion about acceptable work; if code or plans violate it, refactor instead of working around it.

## Design Rules

- Prefer entity-based architecture; put behavior on the type that owns the data, lifecycle, or invariant.
- Model real domain concepts as explicit entity types when they own state, invariants, or behavior.
- Prefer small cohesive types with clear responsibilities over broad manager APIs.
- Prefer clean, general, composable APIs over narrow methods tailored to one caller or one current call path.
- Use simple, clear names for public and internal APIs; avoid names that encode implementation details, transient benchmark work, compatibility shims, or a single use site.
- Make invalid states hard to express with `NonZero*`, `NonNull`, named domain types, and checked construction.
- Avoid tuple structs with unnamed fields for domain entities; use named fields when field meaning matters.
- Avoid free helper functions unless they remove real duplication or express a cross-entity operation.
- Avoid passive adapter, wrapper, or compatibility layers unless they encode a real invariant or remove meaningful duplication.
- Avoid callback-style helper patterns for ordinary control flow; prefer direct calls and explicit results.
- Keep code and architecture simple; introduce abstractions only when they reduce complexity or clarify invariants.

## API Policy

- No backward compatibility is required for public or internal APIs.
- Prefer modifying existing APIs over adding new methods; reshape, rename, delete, or refactor directly instead of growing parallel surfaces.
- Avoid compatibility shims throughout the codebase.
- Review API shape repository-wide when architectural feedback applies; do not fix only the call site where the issue was noticed.
- During planning and implementation, critique the design against idiomatic Rust, allocator invariants, composability, and overfitting before treating it as done.

## v0.1 Scope

- Build only: Linux x86_64, Rust stable, `GlobalAlloc`, one global lock around `Heap`.
- Include mmap-backed runs for small size classes, mmap-backed extents for dedicated allocations, out-of-line metadata, page-indexed pointer lookup, run block-boundary checks, extent exact-pointer checks, basic `realloc`, basic `alloc_zeroed`, and randomized tests.
- Do not add profiles, thread-local heaps, remote frees, quarantine, canaries, hugepages, NUMA, C ABI support, ML placement, or dashboards yet.

## Core Invariants

- Every returned pointer maps to exactly one page-map entry.
- Runs own one mapping and divide it into fixed-size reusable blocks from one size class.
- Extents own one mapping dedicated to exactly one returned allocation.
- Every free must map back to a known entry: run frees must be valid block boundaries, and extent frees must be the exact returned pointer.
- Correctness comes before speed.

## Architecture

Use this first:

```text
GlobalAlloc
  -> Allocator
      -> Heap
      -> PageMap
      -> RunHeap
          -> RunArena
      -> ExtentHeap
          -> ExtentArena
      -> Run
          -> FreeBitmap
      -> Extent
      -> OsMemory
```

## Rust Rules

- Use `#![deny(unsafe_op_in_unsafe_fn)]`.
- Keep unsafe code small, explicit, local, and adjacent to the safety reasoning.
- Prefer methods on `Allocator`, `Heap`, `RunHeap`, `ExtentHeap`, `RunArena`, `ExtentArena`, `Run`, `Extent`, `PageMap`, `OsMemory`, and `SizeClasses`.
- Avoid allocator-internal `Vec`, `Box`, `HashMap`, `String`, formatting, or panic paths unless recursion risk is addressed.
- Abort on invalid frees in v0.1.
- Do not unwind across allocator boundaries.
- Do not add test-only methods to production `impl` blocks; tests inside the owning module can inspect private state directly.
- Avoid lint workarounds that reduce code quality; do not use `#[allow]` or `#[expect]` when a cleaner design or refactor is reasonable.

## References

- `allocator-refs/` is read-only inspiration for tests, invariants, workload shapes, and benchmark categories; do not copy implementation code.
- `ROADMAP.md` owns project direction and milestone boundaries.

## Issue Tracking

- If an agent notices a real issue, critique, or improvement outside the current task, create or update a GitHub issue instead of expanding scope.
- Keep follow-up lists in GitHub issues, not in this file, unless they are durable project policy.

## Commands

| Task | Command |
|------|---------|
| Check | `cargo check --workspace` |
| Test | `cargo test --workspace` |
| Format | `cargo fmt --all` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
