# rseq-rs

Safe Linux [restartable sequence](https://google.github.io/tcmalloc/rseq.html)
word ops. Package `rseq-rs`, lib `rseq_rs`. librseq in Rust — crate-owned
sequences on a caller-chosen word. Not a runic hit path, not a magazine.

## Registration

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

## Words region

```rust
use rseq_rs::Words;

let words = Words::new(2)?;
let w = words.get(cpu)?;
```

`get` borrows `words`. Embedder field: `unsafe Word::from_raw(ptr, cpu)`.

## Word ops (Linux x86-64)

```rust
use rseq_rs::{Error, Rseq};

let rseq = Rseq::try_new()?;
let t = rseq.bind()?;
loop {
    let cpu = t.cpu_id()?;
    let w = rseq.words()?.get(cpu)?;
    match t.compare_exchange(w, 0, 7) {
        Ok(_) | Err(Error::Miss(_)) => break,
        Err(Error::Abort) => {}
    }
}
```

One attempt per call. Kernel preemption restarts inside the CS. CPU
mismatch is `Err(Abort)` — re-read `cpu_id` and pick again. Compare-miss
is `Err(Miss(current))`. No lock, no CAS.

Stress (ignored): `cargo test -p rseq-rs -- --ignored`.

Isolated word-op bench: `taskset -c 0 cargo bench -p rseq-rs --bench words`.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
