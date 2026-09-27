# AGENTS.md

Scope: `crates/runic/tests/`.

- Expect aborts only in subprocesses; double-free cases are `#[cfg(feature = "safe")]` in both the binary and the test.
- Test `RunicAlloc` through standard collections with checksums; do not assume exact pointer reuse on the shared global heap.
