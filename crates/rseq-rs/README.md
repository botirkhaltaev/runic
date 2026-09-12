# rseq-rs

Safe Linux [restartable sequence](https://google.github.io/tcmalloc/rseq.html)
primitives. Package `rseq-rs`, lib `rseq_rs`. Not a runic hit path.

This crate is a stub. v0.1 lands as stacked PRs:

1. This crate (compile only)
2. `Rseq` / `Thread` / `CpuId` (glibc-registered area)
3. `LockedStacks<T>` and `CpuStacks`
4. `Stacks<T>` (x86_64 RSEQ, one commit store)
5. `Quiesced`, stress tests, isolated benches

See [ROADMAP.md](ROADMAP.md) for the thesis and later releases.

```toml
rseq-rs = { path = "crates/rseq-rs" }
```

`publish = false` until v0.1 benches and stress are green. Do not depend on
this from `runic-core` yet.
