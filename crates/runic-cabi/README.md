# runic-cabi

`runic-cabi` builds `librunic.so`, a Linux x86_64 malloc-family interceptor for `LD_PRELOAD`.

```sh
cargo build -p runic-cabi --release
LD_PRELOAD=target/release/librunic.so ./program
```

The shared object exports:

- `malloc`, `free`, `calloc`, `realloc`
- `posix_memalign`, `aligned_alloc`, `memalign`
- `valloc`, `pvalloc`, `reallocarray`, `cfree`
- `malloc_usable_size`
- matching glibc `__libc_*` allocation entry points

`free(NULL)` is a no-op. Unknown and interior pointers abort. Pointer-returning failures set `errno`; `posix_memalign` returns its error code without changing `errno`.

`realloc` preserves the allocation prefix but does not preserve alignment requested through `posix_memalign`, `aligned_alloc`, or `memalign`.

Use either this interceptor or `runic::RunicAlloc` as `#[global_allocator]` in a process. Do not use both copies of the allocator in one process.

The interceptor is loaded at process startup. Loading it later with `dlopen` is unsupported.
