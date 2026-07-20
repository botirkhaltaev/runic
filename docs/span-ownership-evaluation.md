# Span Ownership Evaluation

Issue: #24

Runic currently models mapped memory through separate domain entities:

- `Run` owns small-allocation geometry and bitmap-backed block state.
- `Extent` owns dedicated allocation metadata.
- `PageMap` owns address lookup publication.
- `Arena<Run>` / `Arena<Extent>` (on `RunHeap` / `ExtentHeap`) own metadata slots.
- `ExtentCache` owns reusable extent mappings.
- `RunHeap` owns empty-run release behavior.
- `Heap` coordinates lifecycle transitions.

This split is explicit and worked for the v0.4 global-lock and retention-policy milestone, but thread-local heaps, remote frees, decay policy, hardening, and hugepage-aware allocation will all increase ownership pressure.

## Evaluation

A span or region entity is useful only if it owns a real invariant. A passive wrapper around `AddressRange`, `Mapping`, and `PageEntry` would add ceremony without reducing risk.

The useful invariant would be:

> one mapped address range is published in `PageMap` as exactly one owner and is backed by exactly one allocator metadata object during its live lifecycle.

Today that invariant is spread across `RunHeap::insert_run`, `ExtentHeap::insert_extent`, `PageMap::insert`, and arena insert/remove paths.

## Candidate Shape

A future region entity should have named state and lifecycle, not a tuple-like wrapper.

Possible shape:

```text
Region
  mapping range
  page range
  owner: Run(id) | Extent(id)
  publication state: unpublished | published
```

The entity would be worthwhile if it owns operations like:

- publish owner into `PageMap`
- rollback metadata insertion if publication fails
- remove owner from `PageMap`
- expose owner and address range for remote-free routing
- encode whether a region may be cached, decayed, purged, or hugepage-backed

## Decision For Now

Do not introduce a span/region implementation yet.

Reasons:

- The current split keeps `Run` and `Extent` invariants direct and testable.
- A region object without ownership of publication/removal would be passive.
- The next required ownership question is remote free routing, not a mechanical wrapper.
- The available-run-list work is still compatible with a later region entity.

## When To Introduce It

Introduce a region/span entity when at least one of these features needs it:

- Thread-local heaps need owner identity on mapped ranges.
- Remote-free queues need to route frees to an owning heap or region.
- Backend decay needs region age, retained-byte accounting, or purge state.
- Hugepage-aware allocation needs segment alignment and lifetime grouping.
- Hardening needs guard-page or metadata-integrity policy per mapped range.

At that point, reshape internal APIs directly. No compatibility layer is needed.

## Required Tests For An Implementation

An implementation PR should include tests for:

- successful run publication
- rollback after failed run publication
- successful extent publication
- rollback after failed extent publication
- removal preserving neighboring `PageMap` entries
- owner lookup returning enough identity for future remote-free routing

## Current Recommendation

Keep the current domain split for now, but design #27 and #25 so owner identity can move into a future `Region` without changing public API.

This keeps the architecture simple today and avoids introducing an entity before it owns enough lifecycle to justify its existence.
