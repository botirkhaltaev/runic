# AGENTS.md

Scope: `crates/runic/tests/`.

- Expect aborts only in subprocesses.
- Smoke-test public `RunicAlloc`; do not assume exact pointer reuse on the shared global heap.
