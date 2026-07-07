# Hugepage-Aware Backend Exploration

Issue: #29

Hugepage support is out of the current correctness milestone. It should be explored only as a backend feature after the basic release policy, owner identity, and hardening boundaries are stable.

## Goal

Evaluate whether 2 MiB-aware mapping and packing improves workloads that are limited by TLB misses, page faults, or large contiguous allocation churn without increasing RSS or fragmentation for unrelated workloads.

## Non-Goals

- no default hugepage use
- no hugepage requirement for correctness
- no NUMA policy
- no C ABI or preload work
- no mixing unrelated lifetimes in one hugepage region without measurements
- no change to page-map lookup semantics

## Candidate Entities

Hugepage behavior should belong to a backend entity that owns mapping lifecycle, not to `Heap` as a broad policy switch.

```text
BackendRegion
  mapping: Mapping
  page_size: BackendPageSize
  owner: RegionOwner

BackendPageSize
  base page: 4 KiB
  huge page: 2 MiB

HugepagePolicy
  disabled
  transparent-hugepage hint
  explicit hugepage attempt with fallback
```

`OsMemory` remains the only layer that calls mmap/munmap or OS advice APIs. A future backend region may ask `OsMemory` for alignment, transparent hugepage advice, or explicit hugepage mappings.

## Separate Cases

Treat these as separate experiments:

1. 2 MiB alignment without hugepage advice.
2. Transparent hugepage advice for large extents.
3. Explicit hugepage mappings with fallback to normal pages.
4. 2 MiB segment packing for runs of similar size classes and lifetimes.

Do not combine these in the first implementation. Each case has different failure modes and measurement requirements.

## Page-Map Invariants

Hugepage-aware mappings must preserve the current lookup contract:

- every returned pointer maps to exactly one page-map entry
- runs still own one mapping or one backend region slice with explicit ownership
- extents still own a dedicated returned allocation
- guard or unused pages must not be published as valid user allocations
- fallback mappings must publish the same page-map shape as successful hugepage mappings

If a backend region contains multiple runs, the page map must identify the correct run owner for every 4 KiB page. Hugepage coverage must not become a coarser ownership entry that hides invalid frees.

## Fragmentation Rules

The first segment-packing design should avoid mixing:

- short-lived and long-lived allocations
- run blocks and dedicated extents
- unrelated size classes with very different reuse behavior
- thread-local regions from different owners before remote-free routing is implemented

Packing should begin with one 2 MiB segment per compatible class group or backend owner, then measure before broadening.

## Fallback Behavior

Hugepage support must be optional:

- transparent hugepage advice failure keeps the mapping usable
- explicit hugepage mmap failure falls back to normal `OsMemory::map`
- fallback does not change allocation validity, alignment checks, or abort behavior
- tests must pass on systems without configured huge pages

## Measurements

Required latency benchmarks:

- `explicit/large_alloc_churn/runic/262144`
- `explicit/large_alloc_churn/runic/1048576`
- `explicit/small_biased_random/runic`
- `threaded/mixed_thread_random/runic/4` after thread-local heaps exist

Required system measurements:

- RSS before and after churn
- minor and major page faults
- dTLB load/store misses when available through `perf stat`
- mmap/munmap count or syscall time if explicit hugepage allocation is tested

Use the existing RSS binary for resident-set checks and same-machine `perf stat` for TLB/page-fault effects.

## First Acceptable Implementation

The first code PR should be an opt-in experiment with:

- a named backend page-size policy
- exact fallback to normal mappings
- tests for fallback and page-map publication
- no behavior change when the policy is disabled
- measured evidence from one targeted workload

Do not make hugepages the default until both latency and RSS behavior are favorable across mixed workloads.
