# rseq-rs Roadmap

Standalone Linux restartable-sequence primitives for Rust. Zero runic
dependency. Package `rseq-rs`, lib `rseq_rs`. The crate name `rseq` is taken
(unrelated DSL).

This is librseq in Rust: crate-owned sequences on a caller-chosen word
plus a CPU check. Not a tcmalloc magazine. Registration and `Words` are
on the tree.

## Thesis

RSEQ lets a thread run a short sequence of ordinary stores that must look
atomic with respect to preemption and migration. The kernel either lets the
sequence finish on the same CPU or jumps to a signed abort IP. It does not
roll stores back and does not save partial progress. The hit is ordinary
stores — no lock, no CAS.

The primitive is that sequence, not an array. librseq is
`cmpeqv_storev(v, expect, new, cpu)`: the caller owns `v`. This crate
owns the instruction range. The public handle is `Thread` plus `Word`
(`NonNull<usize>` and `CpuId`). `Words` is an optional mmap of usizes.

The critical section itself is **not** user Rust. A `Fn` / closure /
proc-macro around safe code cannot be a restartable sequence: the compiler
may spill, reorder, or split it, and the kernel needs an exact
`[start_ip, start_ip + post_commit_offset)` range. The crate owns those
sequences.

Compare-miss is `Err(current)`. Abort is retried inside the crate.

Runic integration is out until v0.1 benches exist and new thread-heavy gates
are named. `#135` was not a fair test of RSEQ: the impl never reached the
tcmalloc hit, and the gate was single-thread pinned churn (per-CPU cannot
win that by design).

## Safe API (v0.1)

Behavior lives on the owning types. No free one-liner wrappers.

```rust
let rseq = Rseq::try_new()?;       // None: kernel / glibc
let t = rseq.bind()?;              // Thread; store in caller TLS
let words = rseq.words()?;         // optional region
let cpu = t.cpu_id()?;
let w = words.get(cpu)?;           // Word { ptr, cpu }

t.compare_exchange(w, expect, new)?;  // cmpeqv_storev
t.fetch_add(w, 1);                    // addv
```

- `Rseq` — process registration. `Copy`. `try_new` is `#[cold]`, once.
  glibc area and CPU count. Not membarrier.
- `Thread` — this thread's `Area`. `Copy`. `bind` is `#[cold]`. Owns
  `compare_exchange` / `fetch_add`. Hit takes `&Thread` so it does not
  reload `__rseq_offset` / `fs:0`.
- `Word` — `Copy`. Pointer plus `CpuId`. librseq's `(v, cpu)`.
- `Words` — optional mmap of one `usize` per possible CPU. `get` is
  address math, not a CS.
- `CpuId` — newtype `u32`. `Thread::cpu_id() -> Option<CpuId>`.

The CS loads `area.cpu_id` and aborts if it is not `word.cpu`. Then it
stores through `word.ptr`.

Embedder field in a larger per-CPU struct: `unsafe Word::from_raw(ptr, cpu)`.
Safety: `ptr` is a live `usize`, used only as this word, and outlives the
ops. Array embedder: `unsafe Words::from_raw(base, cpus)`.

`try_new` is `None` → missing glibc rseq or a zero CPU count. The fallback
is the caller's `AtomicUsize`, not a locked twin in this crate.

`Rseq::fence` is optional. First call registers RSEQ membarrier; word ops
never fence.

Other targets: types exist; `try_new` returns `None`. Dependents compile
everywhere. Word width is `usize` (librseq `intptr_t`).

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
types. v0.1 is the word ops, not another magazine.

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
CS aborts if area.cpu_id != word.cpu. Store goes through word.ptr.
No per-op fence. No rseq_cs clear after commit (kernel clears on preempt).
RSEQ path never locks or CASes. No locked twin in this crate.
Never GlobalAlloc (no Vec / Box / String / HashMap). mmap is the OS boundary.
Cold paths may use OnceLock and File into a stack buffer.
Workspace lints. unsafe_op_in_unsafe_fn deny.
```

Crate-owned `Words` may `mmap` / `munmap`. That is the OS boundary, not
an allocator-internal heap.

## Layout (v0.1)

Workspace member `crates/rseq-rs`. Full RSEQ impl on `linux + x86_64`.

```text
src/lib.rs         re-exports
src/rseq.rs        Rseq::try_new / bind / fence / words
src/thread.rs      Thread, CpuId, compare_exchange / fetch_add
src/words.rs       Word, Words, get, from_raw
src/layout.rs      one usize per CPU; mmap region
src/x86_64.rs      private inline asm! (not pub)
src/cpus.rs        CPU count (File, stack buffer)
src/membarrier.rs  private syscalls (fence only)
src/abi.rs         private Area / SIG
```

`Words::get` is `base + cpu * size_of::<usize>()`. Zeros on crate `mmap`.

`asm!` shape: `.pushsection __rseq_cs,"aw"` + local labels (PIE-safe; no
`global_asm!` outline). `jmp entry; .long SIG; abort: entry:` then
`lea cs(%rip)` into `area.rseq_cs`. Load `cpu_id`; abort if not `word.cpu`.
Compare-exchange or add through `word.ptr`. Committing store last. Abort
retried in Rust. No `cpu_id_start` pre-read and recheck.

## Releases

### v0.1.0 — x86_64 word ops

```text
Rseq / Thread / CpuId / Word / Words
Thread::compare_exchange / fetch_add; unsafe from_raw only
tests: abi (private), smoke, words, ops, stress (ignored)
bench: TLS Cell vs Thread word ops vs AtomicUsize
README + AGENTS.md
```

Stress: threads > cores, `sched_setaffinity` flap + `setitimer` SIGALRM.
Unique add / no lost CAS. A signature bug is SIGSEGV.

Bench is the isolated number. `#135` never isolated it.

### v0.2.0 — aarch64

Same safe API. `adrp`/`add` for `cs`, `mrs tpidr_el0`, aarch64 `SIG`.
CI `cargo check --target aarch64-unknown-linux-gnu`.

### v0.3.0 — more word ops

```text
store_if   // cmpeqv_trystorev_storev
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

### Later — magazine

Index stacks as a layer on these ops (or a dedicated CS if the index-stack
sequence stays tighter than two word ops). Not v0.1.

## Out

```text
User Rust / closures / proc-macros as the critical section
Silent lock or CAS on the RSEQ hit
Locked / atomic twin in this crate
Ops on Words
Runic magazine / heap / PageMap
Porting tcmalloc or snmalloc
crates.io publish until v0.1 benches and stress are green
Runic integration before the isolated bench table exists
```

## Integration (later)

Starts from the v0.1 bench table and new gates: threads > cores, thread
spawn churn, RSS under thread count. Not churn/64. Not a retry of `#135`
as written. Runic would call `Word::from_raw` on a field or own `Words`.
