# AGENTS.md

Scope: `crates/runic-core/src/heap/table/`.

- Keep TLS heap-entry owner-local frontend state and run caches here.
- Keep `HeapSlot` responsible for remote inbox, remote-pending signaling, and abandoned/reactivated lifecycle.
- Keep `HeapTable` responsible for slot identity, generation checks, and lifecycle transitions.
- Do not put `HeapTable` on steady-state owner-local allocation hot paths.
- Clear or validate owner-local caches whenever a heap slot is abandoned or reactivated.
- Preserve explicit separation between owner-local frees and remote-free enqueue/drain behavior.
- Do not introduce passive forwarding wrappers for heap table behavior; prefer methods on `HeapTable`, `HeapSlot`, or the TLS heap entry that owns the state.
