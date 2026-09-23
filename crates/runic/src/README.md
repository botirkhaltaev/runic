# runic/src

Source for the public global allocator wrapper.

## Modules

- `lib`: exports `RunicAlloc`.
- `global`: implements `core::alloc::GlobalAlloc` for `RunicAlloc` by delegating to `runic_core::Allocator`.
- `bin/abort_case`: helper executable used by integration tests that expect process aborts.

Allocator policy, metadata, pointer validation, and mmap ownership live in `runic-core`.
