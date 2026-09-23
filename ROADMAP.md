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

Runic **0.8.0** is published: `runic-core`, `runic-alloc`, `runic-cabi`.

The release includes owner-local heaps, two TLS heap slots, remote free,
thread-exit draining, run and extent retention, pointer-only C free/realloc,
and a malloc-family `LD_PRELOAD` library.

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

## Next: hardening

Planned:

```text
checked or encoded reusable-block metadata
metadata cookies
optional delayed reuse
guard pages for selected large allocations
randomized placement only after deterministic paths are stable
```

Later work: backend region ownership, decay, purge, and hugepage-aware mapping.
