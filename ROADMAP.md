# Runic Roadmap

## Thesis

Runic is a Rust-native hosted allocator with a small auditable unsafe core,
out-of-line metadata, explicit ownership transitions, and tests that exercise
allocator invariants before performance layers hide bugs.

Runic is not a line-for-line port of mimalloc, jemalloc, TCMalloc, snmalloc, or
another allocator. It should learn from those allocators while keeping Runic's
domain model direct, Rust-native, and testable.

The useful claim is not:

```text
Runic is safe because it is written in Rust.
```

The useful claim is:

```text
Runic reduces and audits the unsafe core, encodes allocator invariants in
owned entities, and makes correctness measurable before adding concurrency,
hardening, or backend complexity.
```

Correctness is mandatory (auditable ownership, fail-closed frees). Within that,
performance is the primary design goal: keep hot paths simple and prefer safe
idiomatic Rust, with `unsafe` only where ownership/OS boundaries or measured
hot paths require it. Architecture should stay simple until a new entity owns a
real lifecycle, invariant, or policy.

The owner-local current run is that entity for the small hit. Owner double-free
is undefined; remote admission stays fail-closed. One process-wide payload; TLS
identity is free. The `Run` header lives in the run space (`base + RUN_SIZE`);
small free is `header_of` (mask + raw `base` check).

Measurement history lives in [`diary.md`](diary.md). Do not treat it as current
scope.

## Current Status

Latest published release: `0.6.0`.

The tree ships the v0.6 owner-local heap frontend: TLS heaps own runs and
extents that store their process-lifetime `&Heap` owner and derive `HeapId`,
private run claim-bitmap remote admission,
run/extent `Inbox` coalesced by owner, and Draining lifecycle after thread exit,
with explicit page-map ownership. Heap lifecycle lives on `Heaps` / `Heap`
(Heaps indexes each Heap; each `Heap` owns inboxes and `RunHeap`/`ExtentHeap`).

Owner-local hit is a TLS current run per class (pop). Small miss/realloc uses
`Run::header_of`; its raw base check precedes pointer construction. `locate` is
offset from the run base. Run mappings are `RUN_SIZE`-aligned; the header and
claim tail sit after the payload. `Run::allocate` is pop only; `extend` on miss.
Owner free hit is `Run::free`; `push_available` is miss / slow / unbind. One
process-wide payload; `Allocator::ctx()` is the handle. A Draining heap may be
`adopt`ed by the first remote freer (`Draining` → `Active`).

`Heaps::get` is a lock-free `Arena` read. Live counts are atomics on `Heap`,
confirmed by run/extent arena scans during reclaim. Default extent policy is Keep; empty-run Discard is opt-in
`madvise` on the payload. Zeroed Keep reuse ≥64 KiB discards pages without the
Discard-insert clean flag; below that, memset.

## Supported Scope

Build only:

```text
Linux x86_64
Rust nightly (`#[thread_local]` `THREAD_HEAPS`)
GlobalAlloc
owner-local heaps via Heaps / ThreadHeaps
heap-owned 2 MiB run maps (16 spaces) for size-classed allocations
mmap-backed extents for dedicated allocations (heap-local)
out-of-line metadata
page-indexed pointer lookup (run publish is the 64 KiB payload only)
per-size-class available run lists
pointer freelist + extend on runs (owner DF undefined)
private run claim-bitmap for remote admission (issued + try_set; no per-block Free byte)
run/extent Inbox coalesced by owner (Treiber stack of runs/extents, not per-block nodes)
configurable extent mapping retention and reuse
runs retained for the heap lifetime; Discard is madvise on empty payload
run block-boundary checks
extent exact-pointer checks
basic realloc
basic alloc_zeroed
randomized tests
```

Do not build yet:

```text
profiles
quarantine
canaries
hugepages
NUMA
C ABI
LD_PRELOAD
ML/lifetime placement
stats dashboard
background purge
```

## Core Invariants

```text
Every returned pointer maps to exactly one page-map entry.
A run owns one size class and one range in a heap-owned map.
Extents own one mapping dedicated to exactly one returned allocation.
Every free must map back to a known entry.
Run frees must be valid block boundaries.
Extent frees must be the exact returned pointer.
Cached mappings are not live allocations.
Cached blocks must have exactly one owner and must not be accepted as stale user frees.
```

If these invariants are wrong, thread-local heaps, remote frees, hardening, and
hugepage-aware allocation will hide correctness bugs. If they are right, the
allocator can be made faster without guessing.

## Architecture

```text
GlobalAlloc
  -> RunicAlloc
      -> Allocator          // const handle; ctx() borrows Process
          -> Process { pages: PageMap, heaps: Heaps }  // mmap; not returned
              -> Heaps { Arena<Heap>, free list, config }
                  -> ThreadHeaps
              -> Heap { HeapState, Inbox, Mutex<HeapInner> }
                  -> HeapInner { RunHeap, ExtentHeap }
                  -> RunHeap { Arena<&'static Run>, available[] }
                  -> ExtentHeap { Arena<Extent>, cache }
              -> Run
              -> Extent
              -> OsMemory
```

`Heaps::get` is a lock-free `Arena` read: `len` Acquire, chunk pointer Acquire,
then `Heap::matches` (slot + generation). Occupied slots never move. Active enqueue uses `HeapState`
leases (lease before new `try_queue`). Arena grow covers mapping ownership and
bump insert only. Draining exclusivity is `Mutex<HeapInner>` via
`Heaps::{enqueue,free,flush,reclaim}` (not the arena grow lock across flush).
Adoption takes that mutex before its Draining→Active CAS; reclaim advances the
generation with a CAS so it cannot overwrite an adoption winner.
Shared `&Heap` is atomics-only; Active body mutation is `ThreadHeaps` +
`require_inner`; reclaim is `Heap::reclaim` through `Heaps`. `Allocator::ctx()`
is the only handle into the process payload. Same-thread small-run hits use
TLS-owned heap metadata with no locks or atomics. `PageMap` stays outside heaps
arena locks so dealloc lookup is not heaps-locked.

## Entity Responsibilities

```text
RunicAlloc     owns the Rust GlobalAlloc boundary.
Allocator      owns the core public allocator API, abort, and cold unbound routing.
AllocatorCtx   carries process-lifetime PageMap + Heaps references for miss / bind / unbind / body / Draining.
Process        owns the process-wide mmap payload (PageMap + Heaps); not returned.
Heaps          owns `Arena<Heap>`, the Free-heap freelist, owner `unbind`, and Draining `enqueue` / `free` / `flush` / `reclaim`.
Heap           owns HeapState, Inbox, and `Mutex<HeapInner>`; shared surface is atomics only (`enqueue` / mode).
HeapInner      owns RunHeap / ExtentHeap (exclusive metadata).
Arena          owns published immovable slots (`get` lock-free; `push` shared; `vacant` / `insert` / `remove` exclusive).
LayoutSpec     owns normalized layout semantics.
SizeClasses    owns size-class selection.
OsMemory       maps anonymous pages; Mapping owns the mmap lifecycle (Drop munmaps).
PageMap        owns page-indexed lookup and returns borrowed Run / Extent owners.
RunHeap        owns Arena<&'static Run> (in-space headers), run checkout (acquire), and available run lists.
Run            owns in-page header, owning `&Heap`, pointer freelist + extend + live, claim bitmap, and embedded Link<Run>. Owner DF undefined.
ExtentHeap     owns Arena<Extent>, dedicated allocation policy, and mapping reuse.
ExtentCache    owns an intrusive ExtentId list of retained extents and exact-budget reuse.
Extent         owns dedicated allocation metadata, owning `&Heap`, embedded Link<Extent>, and Claimed byte state.
ThreadHeaps    owns equal TLS `ThreadHeap` slots, current[class], and the sole Active body path.
ThreadHeap     is one TLS-owned process heap: Vacant or Active (`&Heap` + captured HeapId).
```

Prefer direct methods on the entity that owns the state. Do not add passive
wrappers, compatibility shims, or test-only methods to production impl blocks.

## Workspace

```text
crates/runic-core
  allocator mechanics and core state; published as runic-core

crates/runic
  public GlobalAlloc wrapper; published as runic-alloc, imported as runic

crates/runic-test-support
  reusable test support

crates/runic-bench
  Criterion `global_*` application workloads
```

## Tests

Default tests should cover:

```text
layout normalization and overflow checks
size-class alignment invariants
run freelist / claim-bitmap / locate behavior
mmap mapping and writability
run block uniqueness and boundary checks
run arena reservation, insertion, mutation, removal
run cache retention and reuse policy
extent cache retention, eviction, and reuse policy
page-map lookup, removal, overlap rejection, L2 boundary crossing
small and large allocation paths
alignment matrices
alloc_zeroed
realloc prefix preservation and in-place growth
subprocess abort cases
Box, Vec, String, HashMap, Arc smoke tests
deterministic randomized allocation traces
Active lease remote free and Draining late free
thread-exit / never-bound freer claim→enqueue (no TLS batch)
```

Abort tests must run in subprocesses, not inside the test harness process.

## Benchmarks

Use application workloads to choose architecture, not to justify special cases.

```text
cargo bench -p runic-bench --no-run
scripts/profile.sh
```

Each retained workload lives in `crates/runic-bench/src/workloads/`. Profile
before and after with `scripts/profile.sh`. Retain a change only when the
Criterion corpus improves and no workload regresses more than 1%. Do not add
synthetic `GlobalAlloc` ports or lifecycle probes to win a number.

Competitor crates in `runic-bench` (defaults, no extra features):

```text
snmalloc-rs 0.3.x   cmake Release -O3, initial-exec TLS; native-cpu off
mimalloc 0.1.x      v3, MI_SECURE off, initial-exec TLS
jemalloc 0.6.x      background_threads_runtime_support only
```

`[profile.bench]` is `lto = "fat"`, `codegen-units = 1`. Cost matches ordinary
`cargo bench` (no forced frame pointers).

## Milestones

### v0.3 Released: Optimized Global-Lock Core

```text
out-of-line run and extent metadata
page-indexed owner lookup
available run lists
basic realloc and alloc_zeroed
randomized traces and abort-case tests
```

`tag: 0.3.0` — `runic-core` / `runic-alloc` 0.3.0.

### v0.4 Released: Retention Policy And Ownership Cleanup

```text
AllocatorConfig and ExtentConfig
ExtentPolicy::{Keep, Discard, Unmap} with exact-length reuse
ExtentCache intrusive head list, exact slot and byte budgets
page-map publication/removal invariants for cached mappings
runs retained by default (empty-run OS release not shipped)
```

`tag: 0.4.0`.

### v0.5 Released: Full Thread-Local Heaps

```text
HeapId ownership on Run and Extent (no Owner/root heap)
ThreadHeap frontend
run/extent Inbox coalesced by owner (claim → enqueue → accept)
private run claim-bitmap remote admission
thread-exit Draining mode
heap-local extents
grow-on-demand metadata arenas
single Allocator::abort sink
```

`tag: 0.5.0` — `runic-core` / `runic-alloc` 0.5.0.

### v0.6 Released: Owner-Local Hit

```text
TLS current run per class; Run::allocate is pop only
#[thread_local] THREAD_HEAP
owner free hit is Run::free; push_available is miss / slow / unbind
in-page Run header (header_of)
remote-free remain fail-closed
```

`tag: 0.6.0` — `runic-core` / `runic-alloc` 0.6.0.

### Next: Hardening

Strengthen corruption and misuse detection after ownership routing is explicit:

```text
checked or encoded reusable-block metadata
metadata cookies
optional delayed reuse
guard pages for selected large allocations
randomized placement only after deterministic paths are stable
```

Later: backend region ownership, decay, purge, and hugepage-aware mapping.

## Reference Lessons

Use `allocator-refs/` as read-only inspiration:

```text
linked-list-allocator: minimal Rust GlobalAlloc shape and alignment tests
talc: Rust-native allocator structure and high-alignment regressions
ferroc: randomized allocation traces and zeroed allocation checks
mimalloc: page-local free-list and locality lessons
TCMalloc: frontend/middle/backend layering and size-class tests
snmalloc: remote-free/message-passing design
PartitionAlloc, Scudo, hardened_malloc: hardening and metadata boundaries
mimalloc-bench: workload and benchmark ideas
```

Do not copy reference implementation code.

Standalone librseq-in-Rust lives in
[botirkhaltaev/rseq-rs](https://github.com/botirkhaltaev/rseq-rs). Do not wire
it into the allocator hit; per-CPU heaps on it were measured and declined
([diary.md](diary.md)).

## Standing Rules

```text
No backward compatibility is required for public or internal APIs.
Prefer reshaping existing APIs over adding parallel methods.
Keep names simple and domain-specific.
Keep allocator-internal caches allocation-free.
Do not add allocator-internal Vec, Box, HashMap, String, formatting, or panic paths
unless recursion risk is explicitly addressed.
Do not add hardening or hugepage support before the milestone that owns the required invariants.
Track follow-up ideas in GitHub issues or diary.md, not as drive-by scope.
```
