# runic-preload

`runic-preload` starts child processes with `librunic.so` ahead of libc. It is
an internal test crate and is not published.

```sh
cargo test -p runic-preload
```

Cargo builds the interceptor as a cdylib artifact dependency and provides its
path through `LIBRARY`.

## Layout

- `src/lib.rs`: the interceptor path.
- `src/bin/preload-case.rs`: one case per process.
- `tests/preload.rs`: runs each case preloaded and checks the exported symbols.

## Child process

`LD_PRELOAD` applies at `exec`, so it cannot replace the running test harness's
allocator. Invalid-pointer cases also abort the process. One fixture binary
runs both kinds of case. `runic-cabi` unit tests cover entry-point contracts
directly.

## Cases

- `interposed`: `malloc` resolves to `librunic.so`, and blocks survive growth.
- `threads`: threads exit while still bound to a heap.
- `unknown`, `small-interior`, `large-interior`, `realloc-interior`: abort.

Unknown `RUNIC_*` values leave that setting at its default.
