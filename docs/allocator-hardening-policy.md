# Allocator Hardening Policy

Issue: #28

Runic's current hardening boundary is correctness-first detection:

- unknown pointers fail `PageMap` lookup
- run frees must land on block boundaries
- extents must be freed with the exact returned pointer
- run block state detects double frees and realloc-after-free
- allocator public boundaries abort instead of unwinding

This is the baseline. New hardening must strengthen detection without hiding broken ownership invariants.

## Ordering

Implement hardening in this order:

1. Encoded run free-list links.
2. Metadata cookies for run and extent records.
3. Optional delayed reuse for selected small classes.
4. Guard pages for selected large allocations.
5. Randomized placement only after deterministic allocator paths are stable.

Do not add quarantine, randomized placement, or guard pages before owner identity and remote-free routing are represented in code. Those features can otherwise obscure whether a pointer belongs to the local heap, remote owner, cached extent, or released mapping.

## Profiles

Runic does not have allocator profiles yet. Until profiles exist, hardening should be compiled as a fixed policy per PR, not runtime configuration.

Future profiles should be explicit, for example:

- baseline correctness
- hardened debug
- hardened production

Do not add environment-variable parsing or string-based configuration inside allocator internals.

## Feature Gates

### Encoded Free-List Links

Free-list encoding is the first acceptable hardening feature because `FreeList` owns the stored next pointer.

Acceptance:

- encoding and decoding are local to `FreeList`
- corruption has a focused detection path
- no allocator-internal allocation
- small churn overhead is measured

### Metadata Cookies

Metadata cookies belong on the entity whose identity they protect: `Run`, `Extent`, or a future `Region`.

Acceptance:

- cookie validation happens before metadata mutation
- mismatch returns a domain error that aborts at `Allocator`
- tests directly corrupt test-owned metadata or use a controlled internal constructor

### Delayed Reuse

Delayed reuse must be represented as explicit block state, not inferred from a side queue.

Acceptance:

- block states distinguish user allocated, free-list eligible, and delayed reuse
- queue capacity is fixed
- overflow behavior is deterministic

### Guard Pages

Guard pages belong in `OsMemory` or a backend mapping entity that owns mmap/munmap lifecycle.

Acceptance:

- page-map publication excludes guard pages
- extent exact-pointer checks remain unchanged
- RSS and page-fault cost are measured

### Randomized Placement

Randomized placement must not replace deterministic tests.

Acceptance:

- seed ownership is explicit
- tests can force deterministic selection
- run block-state validation is unchanged

## Testing

Every hardening PR must add a focused test for the intended detection path.

Existing abort-case tests must continue to pass:

- unknown free
- run interior free
- extent interior free
- run double free
- realloc with an interior pointer
- realloc after free

Hardening tests should not rely on formatting, panic messages, or unwinding.

## Measurement

Every hardening PR must report overhead for:

- `explicit/single_size_churn/runic/64`
- `explicit/small_biased_random/runic`
- `explicit/large_alloc_churn/runic/262144`
- `explicit/realloc_growth/runic`

If a feature affects RSS or page faults, also run the RSS tool and `perf stat` on the relevant benchmark binary.

## Non-Goals For The Current Milestone

- no quarantine by default
- no guard pages by default
- no randomized placement by default
- no runtime hardening configuration
- no panic or formatting path in allocator internals

The next hardening implementation should be encoded free-list links, because that feature has a clear owner and does not require changing heap ownership or backend mapping policy.
