# rseq-rs Roadmap

Standalone Linux restartable-sequence primitives for Rust. Zero runic
dependency. Package `rseq-rs`, lib `rseq_rs`. The crate name `rseq` is taken
(unrelated DSL).

Crate stub is on the tree. This file is the thesis and release plan.

## Thesis

RSEQ lets a thread run a short sequence of ordinary stores that must look
atomic with respect to preemption and migration. The kernel either lets the
sequence finish on the same CPU or jumps to a signed abort IP. It does not
roll stores back.

The crate is a **safe, idiomatic Rust API** over that kernel contract.
Callers see owning types, `Option` / `Result`, RAII, and generics. They do
not see `asm!`, offsets, signatures, or `syscall`. `unsafe` exists only at
OS and foreign-layout boundaries (`from_raw` for an embedder that already
owns the region).

The critical section itself is **not** user Rust. A `Fn` / closure /
proc-macro around safe code cannot be a restartable sequence: the compiler
may spill, reorder, or split it, and the kernel needs an exact
`[start_ip, start_ip + post_commit_offset)` range. The crate owns those
sequences. The public methods are safe because they only run those
sequences on memory the crate (or `from_raw`) already validated.

Miss / full / unavailable are ordinary Rust: `Option`, `Result`,
`unwrap_or_else`. Closures belong on that path, not inside the CS.

Runic integration is out until v0.1 benches exist and new thread-heavy gates
are named. `#135` was not a fair test of RSEQ: the impl never reached the
tcmalloc hit, and the gate was single-thread pinned churn (per-CPU cannot
win that by design).

## Safe API (v0.1)

Behavior lives on the owning types. No free one-liner wrappers.

```rust
let rseq = Rseq::try_new()?;              // None: kernel / glibc / membarrier
let stacks = rseq.stacks::<T>(cap)?;      // crate-owned per-CPU region
let t = rseq.bind()?;                     // Thread; store in caller TLS

let p = stacks.pop(&t).ok_or_else(|| refill())?;
stacks.push(&t, p).inspect_err(|_| overflow(p))?;

let mut q = stacks.quiesce(cpu)?;         // stop + fence; Drop restores
for p in q.drain() { /* exclusive */ }
```

- `Rseq` — process registration. `Copy`. `try_new` is `#[cold]`, once.
- `Thread` — this thread's `Area`. `Copy`. `bind` is `#[cold]`. Hit methods
  take `&Thread` so they do not reload `__rseq_offset` / `fs:0`.
- `Stacks<T>` — typed per-CPU index stacks the crate `mmap`s and `Drop`s.
  `pop` → `Option<NonNull<T>>`, `push` → `Result<(), Full<T>>` (returns the
  item), batch ops take `&mut [NonNull<T>]` and return `usize` committed.
- `Quiesced<'_, T>` — RAII drain of one CPU. Exclusive `&mut` to `current`
  and slots. `Drop` publishes `current` and restores `capacity` (release).
- `CpuId` — newtype `u32`. `Thread::cpu_id() -> Option<CpuId>`.

Embedder path (runic later): `unsafe Stacks::from_raw(layout)`. Safety:
region is live, sized, and exclusively used as this crate's header+slots
for `T`. Default path does not need this.

Other targets and failed `try_new`: types exist, constructors return
`None` / `Err(Unavailable)`. Dependents compile everywhere.

### Locked backend (opt-in type, not a hidden fallback)

The RSEQ path never locks and never CASes. When `Rseq::try_new` fails, the
caller constructs a **different type** with the same methods:

```rust
let stacks = LockedStacks::<T>::new(cpus, cap)?;
let p = stacks.pop()?;
stacks.push(p)?;
```

Same `pop` / `push` / `quiesce` names. A `CpuStacks<T>` trait (in the crate)
lets a generic caller pick `Stacks<T>` or `LockedStacks<T>` at the type
level. No runtime branch on the RSEQ hit. Covers gVisor, `rseq=0`, old
kernels without lying about the fast path.

## What #135 got wrong

Tried in [runic#135](https://github.com/botirkhaltaev/runic/issues/135),
reverted. Recorded as churn/64 65.3 vs TLS magazine 43.6. Three impl bugs
and one thesis bug:

1. Intrusive two-store list (payload link, then head). Abort between stores
   is not restartable. Forced `skip_head`, then a `busy` CAS — a lock that
   defeats RSEQ. Fan-in double-freed.
2. Index-stack reshape still rebuilt `rseq_cs` on the stack every pop/push
   (148 → 225 ins/elem). Result 69.3.
3. Static `__rseq_cs` landed, but pop/push stayed outlined, each op paid
   `SeqCst` + `__rseq_offset` + `fs:0`, and free still walked run metadata
   before the CS. Result 65.3. Fan-in still aborted: take/drain had no
   membarrier quiesce.
4. The gate was `taskset -c 0` single-thread churn. A perfect per-CPU pop
   ties a TLS pop plus the `rseq_cs` install. tcmalloc uses per-CPU slabs
   so cache count scales with cores, not threads.

`toccata-core` and `rsmalloc` each reimplemented the same layer as raw
asm + offsets. This crate keeps that CS shape and hides it behind safe
types.

## Host facts (this machine)

- Kernel 6.12, glibc 2.34 with the RHEL 9 rseq backport.
- `__rseq_offset` / `__rseq_size` / `__rseq_flags` live in `ld.so`.
- glibc registers a 20-byte area (`node_id` / `mm_cid` not populated).
- Self-register via `SYS_rseq` returns `EINVAL`. v0.1 reuses glibc's area.

## Invariants

```text
Safe public API. unsafe only: from_raw, and the private asm.
One committing store, last. Extra stores before it must be scratch.
Static rseq_cs in __rseq_cs ("aw"), 32-byte aligned. Hit stores the pointer.
Caller-owned Thread. Hit does not load __rseq_offset or fs:0.
No per-op fence. No rseq_cs clear after commit (kernel clears on preempt).
RSEQ path never locks or CASes. LockedStacks is a separate type.
Quiesce is capacity = 0, then membarrier(PRIVATE_EXPEDITED_RSEQ, cpu).
No crate-internal Vec / Box / HashMap / String / panic / format.
#![no_std]. libc only. Workspace lints. unsafe_op_in_unsafe_fn deny.
```

Crate-owned `Stacks<T>` may `mmap` / `munmap`. That is the OS boundary, not
an allocator-internal heap.

## Layout (v0.1)

Workspace member `crates/rseq-rs`. Full RSEQ impl on `linux + x86_64`;
`LockedStacks` everywhere.

```text
src/lib.rs         re-exports; cfg gate
src/rseq.rs        Rseq::try_new / bind / fence
src/thread.rs      Thread, CpuId
src/stacks.rs      Stacks<T>, Full<T>, Quiesced, CpuStacks
src/locked.rs      LockedStacks<T> (mutex per CPU, same trait)
src/layout.rs      header + slots; from_raw contract
src/x86_64.rs      private inline asm! (not pub)
src/cpus.rs        parse possible CPUs (no alloc)
src/membarrier.rs  private syscalls
src/abi.rs         private Area / Cs / SIG
```

Internal layout (not pub except via `from_raw` docs): block =
`base + (cpu << shift)`, `Header { current, capacity }`, slot `i` at
`slots + i * size_of::<*mut T>()`. `capacity == 0` is stopped or empty
init: pop misses, push is `Full`.

`asm!` shape: `.pushsection __rseq_cs,"aw"` + local labels (PIE-safe; no
`global_asm!` outline). `jmp entry; .long SIG; abort: entry:` then
`lea cs(%rip)` into `area.rseq_cs`. No bounded-retry counter. No
`cpu_id_start` pre-read and recheck.

## Quiesce

```text
Drainer: capacity = 0
Drainer: membarrier(PRIVATE_EXPEDITED_RSEQ, FLAG_CPU, cpu)
Kernel:  if IP in CS, jump abort_ip
Hitter:  restart, load header, capacity 0 → None / Full
Drainer: drain slots under Quiesced
Drop:    current = leftover, capacity = cap (release)
```

Caller serialises drainers per `(cpu, stacks)` if several exist.

## Releases

### v0.1.0 — x86_64, safe API

```text
Rseq / Thread / Stacks<T> / Quiesced / LockedStacks<T> / CpuStacks
safe pop / push / batch; unsafe from_raw only
tests: abi (private), smoke (skip when Unavailable), stacks, stress, quiesce
bench: pinned cycles/pair, TLS Cell vs Stacks vs LockedStacks vs AtomicU32
README + AGENTS.md
```

Stress: threads > cores, unique tokens, `sched_setaffinity` flap +
`setitimer` SIGALRM storm. Assert no dup/loss. A signature bug is SIGSEGV.

Bench is the number runic integration needs. `#135` never isolated it.

### v0.2.0 — aarch64

Same safe API. `adrp`/`add` for `cs`, `mrs tpidr_el0`, aarch64 `SIG`.
CI `cargo check --target aarch64-unknown-linux-gnu`.

### v0.3.0 — word ops

Safe methods on a crate-owned `PerCpu<T: Copy>` (or `from_raw` words),
both arches. Names follow Rust, not librseq C:

```text
compare_exchange   // cmpeqv_storev
fetch_add          // addv
store_if            // cmpeqv_trystorev_storev
```

Still one committing store each. No user closure in the CS.

### v0.4.0 — self-registration

When `__rseq_size == 0` (tunable off, musl, old glibc): weak `__rseq_*`,
per-thread 32-byte area, unregister on thread exit. `node_id` / `mm_cid`
when the registered area is 32 bytes. Nightly only behind a cargo feature.
`try_new` stays safe; this is still an `Unavailable` vs `Ok` split.

### v0.5.0 — cached block overlay (experimental)

tcmalloc's cached block pointer overlaid on `cpu_id_start` so the hit is
load+test instead of shift+add. Self-registration only. Ship only if the
v0.1 bench moves. API unchanged.

## Out

```text
User Rust / closures / proc-macros as the critical section
Silent lock or CAS on the RSEQ hit (LockedStacks is a distinct type)
Runic magazine / heap / PageMap
Porting tcmalloc or snmalloc
crates.io publish until v0.1 benches and stress are green
Runic integration before the isolated bench table exists
```

## Integration (later)

Starts from the v0.1 bench table and new gates: threads > cores, thread
spawn churn, RSS under thread count. Not churn/64. Not a retry of `#135`
as written. Runic would call `from_raw` on arena memory or own `Stacks<T>`.
