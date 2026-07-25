# AGENTS.md

## Project Truth

- Runic is a Rust-native allocator project; build a correct allocator core with explicit, auditable invariants rather than porting another allocator line-for-line.
- Current milestone: v0.5 owner-local heap frontend with heap-owned runs and extents, explicit ownership, remote-free correctness, retained/reused metadata, bounded caches, and profile-backed optimization.
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
- Avoid free helpers and one-line pass-through methods; call the owning entity directly unless the helper removes real duplication or encodes an invariant.
- Avoid passive adapter, wrapper, or compatibility layers unless they encode a real invariant or remove meaningful duplication.
- Avoid callback-style helper patterns for ordinary control flow; prefer direct calls and explicit results.
- Keep code and architecture simple; introduce abstractions only when they reduce complexity or clarify invariants.
- Optimize for the best allocator architecture rather than backward compatibility or temporary migration paths.
- Keep hot paths simple, direct, and minimal-instruction while preserving explicit correctness invariants.
- Separate owner-local and remote-free paths in type APIs; do not hide cross-thread behavior behind broad manager methods.
- Keep exactly one claim → batch → `HeapTable::publish` → flush remote-free protocol; do not grow a second, parallel remote-free implementation alongside it (e.g. an unbatched or lock-scoped shortcut) for `realloc`, tests, or any other caller. `publish` must complete retained batches against `Draining` heaps (late frees after owner exit), not only `Active` inbox enqueue.
- Do not model small or large allocation ownership as a shared/root heap; every run and extent is stamped with a `HeapId`, and sharing uses remote-free coordination or backend reuse.
- Treat caches as allocator-domain ownership structures, not benchmark-specific shortcuts.
- Do not deepen the extent path into a lock-only design; same-thread extent allocate/free must flow through owner-local `ThreadHeap`/`Heap` state, mirroring the run frontend, and only fall back to the table mutex on TLS miss or cross-thread routing.
- After code or API changes, revamp nearest subtree `AGENTS.md` files so rules match the new architecture; rewrite or delete stale bullets.
- Update nearest subtree `README.md` files when module layout, APIs, or invariants they describe changed.

## API Policy

- No backward compatibility is required for public or internal APIs.
- Prefer modifying existing APIs over adding new methods; reshape, rename, delete, or refactor directly instead of growing parallel surfaces.
- Avoid compatibility shims throughout the codebase.
- Review API shape repository-wide when architectural feedback applies; do not fix only the call site where the issue was noticed.
- During planning and implementation, critique the design against idiomatic Rust, allocator invariants, composability, and overfitting before treating it as done.
- Use profiling to choose optimization order; do not add benchmark-specific hacks.

## v0.5 Scope

- Build only: Linux x86_64, Rust stable, `GlobalAlloc`, owner-local heaps, heap-owned small runs, and heap-local extent caches backed by global OS/page coordination.
- Include mmap-backed runs, mmap-backed extents, out-of-line metadata, page-indexed pointer lookup, run block-boundary checks, extent exact-pointer checks, owner-local small hot paths, remote-free coordination, bounded run/extent retention, `realloc`, `alloc_zeroed`, randomized tests, and benchmarks.
- Do not add per-CPU/RSEQ frontends, quarantine, canaries, hugepages, NUMA, C ABI support, ML placement, dashboards, or background purge yet.

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
     -> AllocatorInner { refs, pages: PageMap, table: Mutex<HeapTable> }
     -> OsMemory
     -> HeapTable { generations[], slots: Arena<Heap> }
         -> ThreadHeap
     -> Heap { mode, RunHeap, ExtentHeap, alloc_count, Inbox }
         -> RunHeap { Arena<Run>, available[] } -> Run (HeapId, BlockStates)
         -> ExtentHeap { Arena<Extent>, cache } -> Extent (HeapId)
```

- `AllocatorInner` is the refcounted mmap instance behind lazy `AtomicPtr` init — not a domain entity. `PageMap` stays outside the table mutex so owner-local TLS hits never take that lock.
- `HeapTable` owns slot identity (`acquire`/`retire`/`reclaim`), generation-checked `heap`/`heap_mut`/`mode`, and mode-aware remote `publish` only — not allocate/dealloc routers.

## Allocator Boundary Scars

Durable lessons from past boundary bugs; do not reintroduce these shapes.

- Exactly one abort sink: `Allocator::abort`. `Heap`, `ThreadHeap`, and `AllocatorInner` return domain `Result`s or call `Allocator::abort` directly; do not add a second private `abort()` copy.
- `AllocatorInner` owns refs + `PageMap` + `Mutex<HeapTable>` + self-hosting `Mapping` — not a broad alloc/free/realloc manager. Call `HeapTable` / `Heap` / `ThreadHeap` methods from `Allocator` (and TLS) instead of growing pass-through routers on `AllocatorInner`.
- Never hold `Mutex<HeapTable>` across a user-memory copy (e.g. `realloc`'s `copy_nonoverlapping`). Compose `alloc` / `dealloc` (or lock only for metadata), release, then copy.

## Rust Rules

- Use `#![deny(unsafe_op_in_unsafe_fn)]`.
- Keep unsafe code small, explicit, local, and adjacent to the safety reasoning.
- Prefer methods on `Allocator`, `Heap`, `RunHeap`, `ExtentHeap`, `Run`, `Extent`, `Arena`, `PageMap`, `OsMemory`, and `SizeClasses`.
- Avoid allocator-internal `Vec`, `Box`, `HashMap`, `String`, formatting, or panic paths unless recursion risk is addressed.
- Abort on invalid frees in v0.1.
- Do not unwind across allocator boundaries.
- Do not add test-only methods to production `impl` blocks; tests inside the owning module can inspect private state directly.
- Avoid lint workarounds that reduce code quality; do not use `#[allow]` or `#[expect]` when a cleaner design or refactor is reasonable.

## References

- `allocator-refs/` is read-only inspiration for tests, invariants, workload shapes, and benchmark categories; do not copy implementation code.
- `ROADMAP.md` owns project direction and milestone boundaries.
- File out-of-scope issues on GitHub; keep durable policy here, not follow-up lists.

## Commands

| Task | Command |
|------|---------|
| Check | `cargo check --workspace` |
| Test | `cargo test --workspace` |
| Format | `cargo fmt --all` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
