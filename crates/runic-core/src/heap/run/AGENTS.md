# AGENTS.md

Scope: `crates/runic-core/src/heap/run/`.

- Keep run block-state behavior on `Run` and `BlockStates`.
- Keep small-allocation policy and available-run list behavior on `RunHeap`.
- Keep retained/reused run lifecycle behavior explicit on the type that owns the run metadata.
- Keep metadata storage and reservation behavior on `RunArena`.
- `Run` and `BlockStates` own the `Reusable | Allocated | RemotePending` state machine.
- Remote paths may mark pending; only owner paths may return pending blocks to reusable.
- Do not use locks on owner-local hot paths unless profiling and invariants justify them.
- Do not add caches that make a block reachable from two owners at once.
- Preserve block-boundary validation and double-free detection.
- Add tests beside the run entity that owns the changed invariant.
