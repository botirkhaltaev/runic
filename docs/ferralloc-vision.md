# Ferralloc Vision

Ferralloc exists because Rust should have a serious Rust-native general-purpose allocator.

The project is not about translating an existing C allocator into Rust. A direct port of mimalloc, jemalloc, TCMalloc, snmalloc, or another allocator would be high effort and low leverage unless it created stronger internal invariants. Ferralloc should instead learn from existing allocators and build a Rust-native design around explicit correctness.

The motivation is simple:

```text
Memory-safe systems should have memory infrastructure built with memory-safety discipline.
```

Allocators are part of the trusted computing base. Bugs in allocators can corrupt arbitrary program state underneath `Vec`, `Box`, `String`, `HashMap`, `Arc`, runtimes, FFI buffers, parsers, servers, and databases.

Rust does not make allocator implementation automatically safe. Allocators still require unsafe pointer arithmetic, OS memory mapping, alignment handling, raw memory ownership, and careful concurrency. Rust does help by making unsafe code explicit and by letting the allocator encode more invariants through types, ownership, module privacy, and tests.

The useful claim is not:

```text
Ferralloc is safe because it is written in Rust.
```

The useful claim is:

```text
Ferralloc reduces and audits the unsafe core, encodes allocator invariants explicitly, and makes allocator correctness testable before adding performance layers.
```

The first milestone is intentionally small:

```text
A global-lock Rust allocator that can run real Rust programs and survive randomized allocation traces.
```

Only after that should the project focus on making the span map fast, adding thread-local heaps, supporting remote frees, and hardening behavior.
