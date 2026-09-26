# Compatibility

On Linux `x86_64`, Runic implements `GlobalAlloc` and the malloc-family entry
points listed below. It supports remote free and thread exit and requires
nightly Rust. The default build is Fast. Extent owner double-free aborts in
both builds. `--features safe` also aborts an owner double-free of a small
block. Hardened is not implemented.
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
| Contract | Null `dealloc` aborts. Layout must match what was used to allocate. Unknown and interior pointers abort |
| Double free | Remote `claim` rejects a second free. Extent owner double-free aborts. Small owner double-free is undefined on Fast and aborts with `--features safe` |
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
thread exit, aborts, and the exact export set.

## Threading

| Capability | Status |
|------------|--------|
| Implemented | Owner-local heaps, two equal TLS heaps, remote free, thread-exit Draining, adopt of one Draining heap |
| TLS limit | With both slots full, a third Draining heap stays on `Heaps::free` |
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
