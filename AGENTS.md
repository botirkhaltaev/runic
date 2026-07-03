# AGENTS.md

## Project Context

Ferralloc is a Rust-native allocator project. The goal is not to port an existing allocator line-for-line, but to build a correct Rust allocator core with explicit, auditable invariants.

Current milestone: a global-lock allocator that can run real Rust programs and survive randomized allocation traces.

## User Preferences

- Prefer entity-based architecture and design.
- Avoid free helper functions unless there is a strong reason.
- Put behavior on the type that owns the data or invariant.
- Keep code and architecture simple wherever possible.
- Write idiomatic, performance-aware Rust without premature optimization.
- Introduce abstractions only when they reduce complexity or clarify invariants.

## v0.1 Scope

Build only: Linux x86_64, Rust stable, `GlobalAlloc`, global lock, mmap-backed spans, small size classes, large direct mmap allocations, out-of-line metadata, pointer-to-span lookup, block boundary checks, basic `realloc`, basic `alloc_zeroed`, and randomized tests.

Do not add profiles, thread-local heaps, remote frees, quarantine, canaries, hugepages, NUMA, C ABI support, ML placement, or dashboards yet.

## Core Invariant

Every pointer returned by ferralloc belongs to exactly one span, every span owns blocks of exactly one size class, and every free must map back to a known span and block boundary.

Correctness comes before speed.

## Architecture

Use this first:

```text
GlobalAlloc
  -> Allocator
      -> State
      -> SpanMap
      -> SpanTable
      -> Span
          -> FreeList
      -> OsMemory
```

Use one global lock around `State`.

## Rust Rules

- Use `#![deny(unsafe_op_in_unsafe_fn)]`.
- Keep unsafe code small, explicit, and local.
- Prefer methods on `Allocator`, `State`, `Span`, `SpanMap`, `SpanTable`, `FreeList`, `OsMemory`, and `SizeClasses`.
- Avoid allocator-internal `Vec`, `Box`, `HashMap`, `String`, formatting, or panic paths unless recursion risk is addressed.
- Abort on invalid frees in v0.1.
- Do not unwind across allocator boundaries.

## Allocator References

`allocator-refs/` contains external allocator projects and benchmark suites for inspiration. Treat it as read-only reference material. Use it for test ideas, invariants, workload shapes, and benchmark categories, not copied implementation code.

## Issue Tracking

- If an agent notices a real issue, critique, or improvement that is out of scope for the current task, create or update a GitHub issue instead of expanding scope.
- Current known follow-ups: improve `SpanMap` metadata allocation, add block-state tracking for double-free detection, and revisit `SpanTable` test/production capacity differences.

## Commands

- Check: `cargo check --workspace`
- Test: `cargo test --workspace`
- Format: `cargo fmt --all`
- Lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
