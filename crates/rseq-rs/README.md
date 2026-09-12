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

## Word ops (landed on Linux x86-64)

```rust
let rseq = Rseq::try_new()?;
let t = rseq.bind()?;
let cpu = t.cpu_id()?;
let w = rseq.words()?.get(cpu)?;
t.compare_exchange(w, 0, 7)?;
let prev = t.fetch_add(w, 1);
```

Abort is retried. Compare-miss is `Err(current)`. CS aborts if this thread
is no longer on `w.cpu`. No lock, no CAS.

Still to land: stress, benches.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
