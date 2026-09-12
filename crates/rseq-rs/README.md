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

## Portable stacks (landed)

```rust
use rseq_rs::{CpuId, LockedStacks};

let stacks = LockedStacks::<u8>::new(2, 32)?;
let cpu = CpuId::new(0)?;
stacks.push_cpu(cpu, p)?;
let p = stacks.pop_cpu(cpu)?;
```

`LockedStacks` is a distinct type (TAS per CPU). The RSEQ hit will never
call it.

## RSEQ stacks (landed on Linux x86-64)

```rust
let rseq = Rseq::try_new()?;
let t = rseq.bind()?;
let stacks = rseq.stacks::<u8>(32)?;
stacks.push(&t, p)?;
let p = stacks.pop(&t)?;
```

Still to land: `Quiesced`, stress, benches.

See [ROADMAP.md](ROADMAP.md). `publish = false`.
