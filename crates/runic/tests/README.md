# runic/tests

Integration tests for the public global allocator crate.

## Files

- `global_alloc.rs`: `RunicAlloc` as `#[global_allocator]`. Standard
  collections keep their contents through growth, `vec![0; n]` is zero after
  dirty reuse, collections move between threads and outlive their builders,
  and thread churn leaves the allocator usable.
- `abort_cases.rs`: spawns the `abort-case` binary once per case and checks
  the signal. Null `dealloc`, interior pointers, and (with `--features safe`)
  owner and remote double-frees abort; a never-issued pointer is fatal by
  abort or fault.

## Run

```sh
cargo test -p runic-alloc
cargo test -p runic-alloc --features safe
```
