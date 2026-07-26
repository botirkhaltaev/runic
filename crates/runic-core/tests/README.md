# runic-core/tests

Integration tests for allocator behavior that crosses core entities.

`manual_alloc.rs` exercises allocation, deallocation, alignment, zeroing, realloc preservation, size classes, run-boundary pressure, deterministic randomized traces, thread-exit Draining frees, Active remote free (including never-bound freer TLS batch publish), and remote-free burst liveness.

## Run

```sh
cargo test -p runic-core --test manual_alloc
```
