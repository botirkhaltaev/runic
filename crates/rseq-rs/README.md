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
let _ = rseq.fence(cpu);
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

`get` borrows `words` — keep the region alive. Embedder field:
`unsafe Word::from_raw(ptr, cpu)`.

## Word ops (Linux x86-64)

```rust
use rseq_rs::{Error, Rseq};

let rseq = Rseq::try_new()?;
let t = rseq.bind()?;
let words = rseq.words()?;
loop {
    let cpu = t.cpu_id()?;
    let w = words.get(cpu)?;
    match t.compare_exchange(w, 0, 7) {
        Ok(_) | Err(Error::Miss(_)) => break,
        Err(Error::Abort) => {}
    }
}
```

One attempt per call. Kernel preemption restarts inside the CS. CPU
mismatch is `Err(Abort)` — re-read `cpu_id` and pick again. Do not retry
the same `Word`. Compare-miss is `Err(Miss(current))`. No lock, no CAS.

A bad abort signature is SIGSEGV, not `Error::Abort`.

Stress (ignored): `cargo test -p rseq-rs -- --ignored`.

Use-case benches (one file each). Isolated numbers: `taskset -c 0`.
`counter` / `cached` report a bare-`Word` CS next to the retry-loop caller.

```text
cargo bench -p rseq-rs --bench counter    # librseq addv / per-CPU stats
cargo bench -p rseq-rs --bench cached     # tcmalloc 1-deep cached object
cargo bench -p rseq-rs --bench freelist   # librseq / mempool per-CPU stack
cargo bench -p rseq-rs --bench drain      # tcmalloc FenceCpu + steal
```

Each file reports rseq next to a non-rseq pair (TLS `Cell` and/or `AtomicUsize`).
Non-rseq benches still run if rseq is unavailable.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
