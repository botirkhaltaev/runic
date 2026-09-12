# rseq-rs

Safe Linux [restartable sequence](https://google.github.io/tcmalloc/rseq.html)
word ops. Package `rseq-rs`, lib `rseq_rs`. librseq in Rust — crate-owned
sequences on a caller-chosen word. Not a runic hit path, not a magazine.

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
the kernel or glibc rseq is unavailable — use `AtomicUsize`, not a hidden
lock here. `fence` is optional and registers membarrier itself.

## Words region (landed)

```rust
use rseq_rs::Words;

let words = Words::new(2)?;
let w = words.get(cpu)?;
```

`get` is address math, not a critical section. `unsafe Word::from_raw` for a
field in a caller-owned per-CPU struct.

Still to land: `Thread` word ops, stress, benches.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
