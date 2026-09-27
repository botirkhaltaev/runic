# Compatibility

On Linux `x86_64`, Runic implements `GlobalAlloc` and the malloc-family entry
points listed below. It supports remote free and thread exit and requires
nightly Rust. The default build is Fast, where an owner double-free is
undefined, as in glibc, mimalloc, and snmalloc. `--features safe` aborts an
owner double-free of a small block or an extent and a second remote free of
the same block. Hardened is not implemented.
Hugepage and NUMA knobs exist on payload maps (default Off). Background purge
and telemetry are not implemented. Planned work is [ROADMAP.md](ROADMAP.md).

## Platform

| Support | Status |
|---------|--------|
| Host | Linux x86_64 |
| Rust | Nightly (`#[thread_local]`) |
| Not supported | Other OSes and architectures, WASI |
| `no_std` | Internal to `runic-core` with `c-abi`; not a public deployment target |

## Rust `GlobalAlloc`

| Capability | Status |
|------------|--------|
| Implemented | `alloc`, `dealloc`, `alloc_zeroed`, `realloc` via `RunicAlloc` |
| Contract | Null `dealloc` aborts. Interior pointers abort. A pointer the allocator never issued aborts, or faults on the run-header probe when it arrives with a small layout. A layout that does not match the allocation is undefined on both builds, as in the `GlobalAlloc` contract and mimalloc |
| Double free | Undefined on Fast, as in mimalloc with secure mode off. With `--features safe`: a second owner free of a small block or extent aborts, and a second remote free of the same block aborts |
| `mmap` failure | `alloc` returns null. Allocations that do not need a new mapping still succeed |
| Fork | A single-threaded fork while no allocation is in progress keeps the parent's blocks and allocates in the child. Fork from another thread, or during `alloc` or `free`, is unsupported until `pthread_atfork` (roadmap 0.13) |
| `realloc` | Preserves the prefix; may move. Uses the new `Layout` alignment in both builds |
| Config | `RunicAlloc::new().with_*` (hugepage, NUMA, `ExtentConfig`, `RunConfig`). First `init` in the process wins. Safe is the `safe` Cargo feature, not a config field |

## C malloc family (`runic-cabi`)

Exported from `librunic.so`:

```text
malloc free calloc realloc
posix_memalign aligned_alloc memalign
valloc pvalloc reallocarray cfree
malloc_usable_size
__libc_malloc __libc_free __libc_calloc __libc_realloc
__libc_memalign __libc_valloc __libc_pvalloc
__libc_cfree __libc_posix_memalign
```

| Capability | Status |
|------------|--------|
| Implemented | Symbols above; `free(NULL)` is a no-op; overflowing `calloc` / `reallocarray` return null and set `ENOMEM` |
| Errno | Pointer-returning failures set `ENOMEM` or `EINVAL`. `posix_memalign` returns the code and does not change errno |
| `realloc` | Preserves the prefix. Returns `max_align_t` (16) alignment in both builds, like glibc and mimalloc; `posix_memalign` alignment is not kept |
| Missing | `mallinfo`, `malloc_stats`, `malloc_trim`, `malloc_info`, hooks, per-arena glibc knobs, `dlopen` after startup (roadmap 0.13; hooks stay out) |
| Env | `RUNIC_HUGEPAGE`, `RUNIC_NUMA`, `RUNIC_EXTENT_POLICY`, `RUNIC_EXTENT_SLOTS`, `RUNIC_EXTENT_BYTES`, `RUNIC_RUN_POLICY` at first init |
| Deploy | `LD_PRELOAD` at process start. Do not combine with `#[global_allocator]` `RunicAlloc` in the same process |

`runic-cabi` tests entry-point contracts. `runic-preload` tests interposition,
thread exit, aborts, the exact export set, and real programs (`sh`, `ls`)
running under the preload.

## Threading

| Capability | Status |
|------------|--------|
| Implemented | Owner-local heaps, a TLS list of heaps (alloc heap at the front, adopt at the back), remote free, thread-exit Draining |
| TLS limit | The last attached heap stays, so retained empty runs remain reachable |
| Not planned | Per-CPU heaps and RSEQ on the hit were measured and declined; see [diary.md](diary.md) |

## Retention and reuse

| Capability | Status |
|------------|--------|
| Implemented | Runs retained for the heap lifetime; `RunPolicy::Discard` is `madvise` on empty payload. Extent Keep / Discard / Unmap with exact-length reuse. Payload hugepage Off / Thp and NUMA Off / Local. Defaults Off after the Fast screen |
| Missing | Background purge, decay, idle-time unmap (roadmap 0.12) |

## Hardening and ops

Missing: quarantine, canaries, guard pages, cookies, checksums, delayed
reuse. Hardened is roadmap 0.11. Stats dashboards and `MALLOC_*` compatibility
stay declined. See [ROADMAP.md](ROADMAP.md).
