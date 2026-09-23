# runic-preload

`runic-preload` tests `librunic.so` the way it is actually deployed: loaded
ahead of libc, serving a process it did not compile with.

The package is internal to the workspace and is not published.

```sh
cargo test -p runic-preload
```

Cargo builds the interceptor as a cdylib artifact dependency, so `LIBRARY`
points at the `librunic.so` of the current profile with no path guessing and
no separate build step.

## Layout

- `src/lib.rs`: the interceptor path.
- `src/bin/preload-case.rs`: one case per process.
- `tests/preload.rs`: runs each case preloaded and checks the exported symbols.

## Why a separate binary

`LD_PRELOAD` applies at `exec`, so the interceptor can never replace the test
harness's own allocator, and the invalid-pointer cases end in `abort`. Both
need a child process. Contract coverage for the entry points themselves is in
the `runic-cabi` unit tests, which call them directly.

## Cases

- `interposed`: `malloc` resolves to `librunic.so`, and blocks survive growth.
- `threads`: threads exit while still bound to a heap.
- `unknown`, `small-interior`, `large-interior`, `realloc-interior`: abort.
