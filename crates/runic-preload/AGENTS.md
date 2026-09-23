# AGENTS.md

Scope: `crates/runic-preload/`.

- Internal (`publish = false`). Fixtures only; no allocator logic.
- Take the interceptor path from `LIBRARY`; never derive it from the target directory.
- One fixture binary, one case per process. Add a case, not a binary.
- Cover an entry point here only when preloading is what makes it observable; contract checks belong in the `runic-cabi` unit tests.
- Keep `EXPORTS` exact: an unexpected export replaces a libc symbol in every preloaded process.
- Assert on `ExitStatus::signal`, not on a numeric status.
