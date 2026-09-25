# runic-cabi

`librunic.so`, a Linux x86_64 malloc-family interceptor for `LD_PRELOAD`.

```sh
cargo build -p runic-cabi --release
LD_PRELOAD=$PWD/target/release/librunic.so ./program
```

[mimalloc-bench](https://github.com/daanx/mimalloc-bench):

```sh
alloc_lib_add "runic" "/path/to/runic/target/release/librunic.so"
./bench.sh runic cfrac espresso
```

## Symbols

```text
malloc free calloc realloc
posix_memalign aligned_alloc memalign
valloc pvalloc reallocarray cfree
malloc_usable_size
__libc_malloc __libc_free __libc_calloc __libc_realloc
__libc_memalign __libc_valloc __libc_pvalloc
__libc_cfree __libc_posix_memalign
```

## Behavior

| Call | Result |
|------|--------|
| `free(NULL)` | No-op |
| Unknown or interior pointer | Abort |
| `malloc` / `calloc` / `realloc` failure | Null, `errno = ENOMEM` (or `EINVAL` for bad align) |
| `posix_memalign` failure | Returns `EINVAL` / `ENOMEM`; errno unchanged |
| `realloc` | Prefix preserved. Alignment from `posix_memalign` / `aligned_alloc` / `memalign` is not preserved (new align is 16) |
| `aligned_alloc` | Alignment power of two; size a multiple of alignment (C11) |

Load at process start; late `dlopen` is unsupported. Do not combine it with
`#[global_allocator]` `RunicAlloc` in one process. They share Runic's
process-global allocator state, so whichever boundary initializes it first
fixes the configuration for both.

At first `init`, cabi overlays `RUNIC_*` (`libc` getenv). Unknown values leave
that key at the Fast default. `RunicAlloc::new().with_*` does not read env.

```text
RUNIC_MODE           fast | safe | hardened   (only fast runs; others abort)
RUNIC_HUGEPAGE       off | thp
RUNIC_NUMA           off | local
RUNIC_EXTENT_POLICY  keep | discard | unmap
RUNIC_EXTENT_SLOTS   usize
RUNIC_EXTENT_BYTES   usize
RUNIC_RUN_POLICY     keep | discard
```

This package builds a cdylib, not a Rust library dependency. It enables
`runic-core`'s `c-abi` feature for pthread thread exit.

Unit tests cover C contracts. `cargo test -p runic-preload` covers
interposition and abort. See [Compatibility](../../COMPATIBILITY.md).
