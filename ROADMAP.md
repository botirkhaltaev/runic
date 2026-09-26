# Runic Roadmap

## Thesis

Runic is a Linux allocator written in Rust. Metadata is outside user
allocations, ownership is explicit, and unsafe code is limited to OS and
ownership boundaries or measured hot paths. Tests establish invariants before
concurrency and performance work depend on them.

[Architecture](ARCHITECTURE.md) records the design,
[Compatibility](COMPATIBILITY.md) records the supported surface, and
[diary.md](diary.md) records measurements.

## Current status

Runic **0.9.0** is published: `runic-core`, `runic-alloc`, `runic-cabi`.

The release includes owner-local heaps, two TLS heap slots, remote free,
thread-exit draining, run and extent retention, a malloc-family `LD_PRELOAD`
library, a `Memory` leaf, payload hugepage and NUMA hints, and const
allocator config. Fast defaults are Off/Off.

## Milestones

### v0.3: Optimized global-lock core

```text
out-of-line run and extent metadata
page-indexed owner lookup
available run lists
basic realloc and alloc_zeroed
randomized traces and abort-case tests
```

Released as `0.3.0`.

### v0.4: Retention policy and ownership cleanup

```text
AllocatorConfig and ExtentConfig
ExtentPolicy::{Keep, Discard, Unmap} with exact-length reuse
ExtentCache intrusive head list, exact slot and byte budgets
page-map publication/removal invariants for cached mappings
runs retained by default (empty-run OS release not shipped)
```

Released as `0.4.0`.

### v0.5: Full thread-local heaps

```text
HeapId ownership on Run and Extent (no Owner/root heap)
ThreadHeap frontend
run/extent Inbox coalesced by owner (claim then enqueue then accept)
private run claim-bitmap remote admission
thread-exit Draining mode
heap-local extents
grow-on-demand metadata arenas
single Allocator::abort sink
```

Released as `0.5.0`.

### v0.6: Owner-local hit

```text
TLS current run per class; Run::allocate is pop only
#[thread_local] THREAD_HEAPS
owner free hit is Run::free; push_available is miss / slow / unbind
in-page Run header (header_of)
remote-free remain fail-closed
```

Version 0.6.0 (no repository tag).

### v0.7: Two TLS heaps

```text
two equal TLS heaps; third Draining adopt stays on Heaps::free
immortal extent slots; PageOwner is &'static Run / Extent
Heap live atomics; reclaim scans after
lock-free Heaps::get
draining admit / flush take optional owner
Heaps::enqueue deleted; Active enqueue admits via acquire_lease
real-workload Criterion corpus is the retain gate
rseq-rs is a separate crate; not wired into the hit
```

Released as `0.7.0`.

### v0.8: C ABI

```text
cdylib malloc family plus posix_memalign / aligned_alloc / memalign / malloc_usable_size
glibc __libc_* aliases; valloc / reallocarray / cfree
separate cdylib package so Rust dependents cannot export malloc
Allocator::free recovers the owner via PageMap; C free then tries the current-run hit
Allocator::resize is pointer-only realloc (no guessed Layout / header_of)
C free(NULL) is a no-op; GlobalAlloc dealloc null still aborts
header_of is not used on unknown pointers
```

Released as `0.8.0`.

### v0.9: Mapping and config

```text
Memory trait behind the Os alias; Linux is the only impl
Mapping owns the mmap; prefer applies THP and NUMA-local hints
HugePage Off or Thp; Numa Off or Local; defaults Off/Off
AllocatorConfig with const with_* and no builder
RUNIC_* overlay at preload for hugepage, NUMA, and run/extent policy
```

Released as `0.9.0`.

## Next

0.9 is published. Production still means the Safe feature, Hardened
integrity, reclaim, C trim/inspect, and other `Memory` impls. Fast stays free
of Safe and Hardened work.

Any further Fast-path default (reclaim) needs a pinned real-workload screen.
Weight the representative production workloads; investigate every regression
over 1%, but do not require one candidate to win every secondary workload.
Safe and Hardened are measured as their own columns.

### Modes

`RunicAlloc::new().with_x()`, const, no separate builder type. The build is
the mode: default is Fast, `--features safe` is Safe. There is no `Mode`,
`with_mode`, or `RUNIC_MODE`. Preload reads `RUNIC_*` for hugepage, NUMA, and
run/extent policy. Hardened is its own feature in 0.11 and is not combined
with Safe.

### 0.10 Safe

Safe Rust is best-effort in both builds. A site becomes safe only when it is
off the hit, adds no helper or second API, and keeps the same behavior. Every
remaining `unsafe` names the invariant it relies on. Fast hit `unsafe` stays.
Hardened does not run on Safe.

Fast is the malloc baseline: extent owner double-free aborts (the state byte
must be `Allocated`, as glibc checks large chunks), and small owner
double-free is undefined, as mimalloc and snmalloc leave it by default. Safe
adds the small check the way mimalloc `MI_SECURE=4` does: a cheap filter on
the block's first word, then a capped freelist walk only when that word looks
like a link. `realloc` uses the caller's alignment in both builds; no
researched allocator keeps `memalign` alignment across `realloc`.

### 0.11 Hardened

Cookies, canaries, extent guard pages, delayed reuse / quarantine, metadata
checksums. Randomized placement only after Safe is stable. Nothing on Fast.

### 0.12 Reclaim

Decay, idle unmap, `malloc_trim`, optional background purge off Fast.

### 0.13 Ops and process

`mallinfo` / `mallinfo2`, `malloc_stats`, `malloc_info`. Rust counters from
existing live atomics plus RSS. `pthread_atfork`. Late `dlopen` tested. No
glibc hooks, mallopt, or `MALLOC_*` aliases.

### 0.14 Platforms

Linux aarch64, then macOS and Windows as new `Memory` leaves. Cabi per OS only
when an interceptor is worth shipping. Stable Rust only if Fast does not
regress.

### Declined

Unless a new screen reverses them: per-CPU heaps, RSEQ on the hit, extra TLS
slots, signal-safe malloc, WASI, ML placement, stats dashboards.
