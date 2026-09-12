# rseq-rs

Safe Linux [restartable sequence](https://google.github.io/tcmalloc/rseq.html)
primitives. Package `rseq-rs`, lib `rseq_rs`. Not a runic hit path.

## Registration (landed)

```rust
use rseq_rs::Rseq;

let rseq = Rseq::try_new()?;
let t = rseq.bind()?;
let cpu = t.cpu_id()?;
assert!(cpu.get() < rseq.cpus());
assert!(rseq.fence(cpu));
```

`try_new` / `bind` are `#[cold]`. Store `Thread` in caller TLS. `None` means
the kernel, glibc, or RSEQ membarrier is unavailable — use a different type
later (`LockedStacks`), do not expect a hidden lock here.

Still to land: `LockedStacks`, `Stacks`, `Quiesced`, stress, benches.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
