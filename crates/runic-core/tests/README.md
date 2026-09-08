# runic-core/tests

Integration tests for allocator behavior that crosses core entities.

`manual_alloc.rs` exercises allocation, deallocation, alignment, zeroing, realloc preservation, size classes, run-boundary pressure, deterministic randomized traces, thread-exit Draining frees, Active remote free (including never-bound freer claim→enqueue), and remote-free burst liveness.

`run_reuse.rs` is a separate process so run retention does not share process state with other tests.

## Run

```sh
cargo test -p runic-core --test manual_alloc
cargo test -p runic-core --test run_reuse
```
