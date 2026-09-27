# runic-core/tests

Integration tests against the public `Allocator` API. Every block that changes
hands is filled with a seeded pattern and checked before it is freed, resized,
or read on another thread, so a corrupted or aliased block fails at the point
it is observed.

- `api.rs`: single-thread contracts. Every size class and extent boundary,
  the alignment matrix up to 1 MiB, zero-size layouts, unsatisfiable requests,
  `alloc_zeroed` after dirty reuse, `realloc` prefix preservation through every
  size step, `usable_size`, and no overlap between live blocks.
- `threads.rs`: ownership across threads. Blocks outlive their owner, remote
  frees flow back to an Active or Draining owner from bound and unbound
  freers, and a remote-free burst completes without owner progress.
- `stress.rs`: seeded random traces, single-thread and across a ring of
  threads, in the shape of mimalloc's `test-stress`.
- `common/mod.rs`: class and extent size tables, `Rng`, and the `Block`
  pattern helpers.

## Run

```sh
cargo test -p runic-core --tests
```
