# rseq-rs

Safe Linux [restartable sequence](https://google.github.io/tcmalloc/rseq.html)
word ops. Package `rseq-rs`, lib `rseq_rs`. librseq in Rust — not a runic
hit path, not a magazine.

This crate is a stub. v0.1 lands as stacked PRs:

1. This crate (compile only)
2. `Rseq` / `Thread` / `CpuId` (glibc-registered area)
3. `Words` mmap / `from_raw`
4. `compare_exchange` / `fetch_add` (x86_64, one commit store)
5. Stress tests, isolated benches

See [ROADMAP.md](ROADMAP.md) for the thesis and later releases.

```toml
rseq-rs = { path = "crates/rseq-rs" }
```

`publish = false` until v0.1 benches and stress are green. Do not depend on
this from `runic-core` yet.
