//! Packed heap lifecycle state: generation, mode, and enqueue leases.

use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    allocator::Allocator,
    heap::{HeapError, HeapId},
};

const MODE_SHIFT: u32 = 32;
const LEASE_SHIFT: u32 = 35;
const LEASE_MASK: u64 = ((1u64 << 29) - 1) << LEASE_SHIFT;
pub(super) const MAX_LEASES: u32 = (1 << 29) - 1;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapMode {
    Free = 0,
    Active = 1,
    Draining = 2,
    Retired = 3,
}

impl HeapMode {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Active => 1,
            Self::Draining => 2,
            Self::Retired => 3,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Active),
            2 => Some(Self::Draining),
            3 => Some(Self::Retired),
            _ => None,
        }
    }
}

/// Decoded snapshot of the packed [`HeapState`] word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    pub(super) generation: NonZeroU32,
    pub(super) mode: HeapMode,
    pub(super) leases: u32,
}

/// Packed generation + mode + lease count — sole heap lifecycle authority.
///
/// Linearization / ordering:
/// - Active enqueue admit: successful `acquire_lease` `AcqRel` CAS
/// - Inbox link: head CAS in [`super::inbox::Inbox::link`] (after lease admit)
/// - Active→Draining close: `close` `AcqRel` CAS (preserves lease count)
/// - Draining→Active adopt: Inner lock, then `adopt` `AcqRel` CAS (preserves lease count)
/// - Lease release: `Release` `fetch_sub`; unbind observes zero with `Acquire` loads
/// - Draining→Free reclaim: `bump_or_retire` `AcqRel` CAS; cannot overwrite adoption
/// - Free reactivation: `Release` store of Active; owner identity is the stable `&Heap`
pub(crate) struct HeapState {
    word: AtomicU64,
}

impl HeapState {
    pub(super) const fn new(generation: NonZeroU32, mode: HeapMode) -> Self {
        Self {
            word: AtomicU64::new(Self::pack(generation, mode, 0)),
        }
    }

    const fn pack(generation: NonZeroU32, mode: HeapMode, leases: u32) -> u64 {
        debug_assert!(leases <= MAX_LEASES);
        // These byte widenings stay const without integer casts.
        let [g0, g1, g2, g3] = generation.get().to_le_bytes();
        let mut word = u64::from_le_bytes([g0, g1, g2, g3, 0, 0, 0, 0]);
        word |= u64::from_le_bytes([mode.raw(), 0, 0, 0, 0, 0, 0, 0]) << MODE_SHIFT;
        let [l0, l1, l2, l3] = leases.to_le_bytes();
        word |= u64::from_le_bytes([l0, l1, l2, l3, 0, 0, 0, 0]) << LEASE_SHIFT;
        word
    }

    fn decode(word: u64) -> Snapshot {
        let [g0, g1, g2, g3, m0, l0, l1, l2] = word.to_le_bytes();
        let generation = NonZeroU32::new(u32::from_le_bytes([g0, g1, g2, g3]))
            .unwrap_or_else(|| Allocator::abort());
        let mode = HeapMode::from_raw(m0 & 0b11).unwrap_or_else(|| Allocator::abort());
        let leases = u32::from_le_bytes([m0, l0, l1, l2]) >> 3;
        Snapshot {
            generation,
            mode,
            leases,
        }
    }

    pub(super) fn load(&self) -> Snapshot {
        Self::decode(self.word.load(Ordering::Acquire))
    }

    pub(super) fn store(&self, generation: NonZeroU32, mode: HeapMode, leases: u32) {
        self.word
            .store(Self::pack(generation, mode, leases), Ordering::Release);
    }

    pub(super) fn matches(&self, id: HeapId) -> bool {
        let snap = self.load();
        snap.mode != HeapMode::Retired && snap.generation == id.generation()
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.load().mode
    }

    pub(super) fn generation(&self) -> NonZeroU32 {
        self.load().generation
    }

    pub(super) fn is_retired(&self) -> bool {
        self.load().mode == HeapMode::Retired
    }

    pub(super) fn is_free(&self) -> bool {
        let snap = self.load();
        snap.mode == HeapMode::Free && snap.leases == 0
    }

    pub(crate) fn is_active(&self) -> bool {
        let snap = self.load();
        snap.mode == HeapMode::Active
    }

    pub(super) fn leases(&self) -> u32 {
        self.load().leases
    }

    /// Admit one Active enqueue lease for `id`, or fail if closed / overflow.
    ///
    /// Counts in-flight Active **enqueue** admits only — not inbox depth
    /// (that stays live via claim bits / `has_live`). Does not serialize
    /// concurrent freer bodies.
    pub(super) fn acquire_lease(&self, id: HeapId) -> Result<Lease<'_>, HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.generation != id.generation() || snap.mode != HeapMode::Active {
                return Err(HeapError::InvalidHeap);
            }
            if snap.leases == MAX_LEASES {
                return Err(HeapError::InvalidMetadata);
            }
            let next = Self::pack(snap.generation, snap.mode, snap.leases + 1);
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Lease { state: self });
            }
        }
    }

    fn release_lease(&self) {
        let amount = 1u64 << LEASE_SHIFT;
        let prev = self.word.fetch_sub(amount, Ordering::Release);
        // Underflow would corrupt mode/generation bits — fail closed.
        if (prev & LEASE_MASK) < amount {
            Allocator::abort();
        }
    }

    /// Close Active admission while preserving the in-flight lease count.
    pub(super) fn close(&self, id: HeapId) -> Result<(), HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.generation != id.generation() {
                return Err(HeapError::InvalidHeap);
            }
            match snap.mode {
                HeapMode::Active => {
                    let next = Self::pack(snap.generation, HeapMode::Draining, snap.leases);
                    if self
                        .word
                        .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                HeapMode::Draining => return Ok(()),
                HeapMode::Free | HeapMode::Retired => return Err(HeapError::InvalidHeap),
            }
        }
    }

    /// First remote freer takes Active ownership. Leases stay; loser sees Active.
    pub(super) fn adopt(&self, id: HeapId) -> Result<(), HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.generation != id.generation() {
                return Err(HeapError::InvalidHeap);
            }
            match snap.mode {
                HeapMode::Draining => {
                    let next = Self::pack(snap.generation, HeapMode::Active, snap.leases);
                    if self
                        .word
                        .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                HeapMode::Active | HeapMode::Free | HeapMode::Retired => {
                    return Err(HeapError::InvalidHeap);
                }
            }
        }
    }

    /// Bump generation and set Free, or permanently retire.
    ///
    /// Fails when another lifecycle transition changed `expected`; reclaim must
    /// never overwrite a concurrent Draining → Active adoption.
    pub(super) fn bump_or_retire(&self, expected: Snapshot) -> bool {
        debug_assert_eq!(expected.mode, HeapMode::Draining);
        debug_assert_eq!(expected.leases, 0);
        let next = match expected
            .generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU32::new)
        {
            Some(generation) => Self::pack(generation, HeapMode::Free, 0),
            None => Self::pack(expected.generation, HeapMode::Retired, 0),
        };
        self.word
            .compare_exchange(
                Self::pack(expected.generation, expected.mode, expected.leases),
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// RAII Active enqueue lease. Drop releases the packed count.
///
/// Internal to [`super::Heap::enqueue`] only — taken only when a freer newly
/// queues a run/extent.
#[must_use]
pub(super) struct Lease<'a> {
    state: &'a HeapState,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.state.release_lease();
    }
}
