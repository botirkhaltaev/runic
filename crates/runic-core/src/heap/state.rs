//! Packed heap lifecycle state: generation, mode, retired flag, and enqueue leases.

use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    allocator::Allocator,
    heap::{HeapError, HeapId},
};

const MODE_SHIFT: u32 = 32;
const MODE_MASK: u64 = 0b11 << MODE_SHIFT;
const RETIRED_BIT: u64 = 1 << 34;
const LEASE_SHIFT: u32 = 35;
const LEASE_MASK: u64 = ((1u64 << 29) - 1) << LEASE_SHIFT;
pub(super) const MAX_LEASES: u32 = (1 << 29) - 1;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapMode {
    Free = 0,
    Active = 1,
    Draining = 2,
}

impl HeapMode {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Active => 1,
            Self::Draining => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Active),
            2 => Some(Self::Draining),
            _ => None,
        }
    }
}

/// Decoded snapshot of the packed [`HeapState`] word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    pub(super) generation: NonZeroU32,
    pub(super) mode: HeapMode,
    pub(super) retired: bool,
    pub(super) leases: u32,
}

/// Packed generation + mode + retired + lease count — sole heap lifecycle authority.
///
/// Linearization / ordering:
/// - Active enqueue admit: successful `acquire_lease` `AcqRel` CAS
/// - Inbox link: head CAS in [`super::inbox::Inbox::link`] (after lease admit)
/// - Active→Draining close: `close` `AcqRel` CAS (preserves lease count)
/// - Lease release: `Release` `fetch_sub`; retire observes zero with `Acquire` loads
/// - Free reactivation: `Release` store of Active after metadata rebind under the heaps arena lock
pub(crate) struct HeapState {
    word: AtomicU64,
}

impl HeapState {
    pub(super) fn new(generation: NonZeroU32, mode: HeapMode) -> Self {
        Self {
            word: AtomicU64::new(Self::pack(generation, mode, false, 0)),
        }
    }

    fn pack(generation: NonZeroU32, mode: HeapMode, retired: bool, leases: u32) -> u64 {
        debug_assert!(leases <= MAX_LEASES);
        let mut word = u64::from(generation.get());
        word |= u64::from(mode.raw()) << MODE_SHIFT;
        if retired {
            word |= RETIRED_BIT;
        }
        word |= u64::from(leases) << LEASE_SHIFT;
        word
    }

    fn decode(word: u64) -> Snapshot {
        let retired = word & RETIRED_BIT != 0;
        let generation = NonZeroU32::new(u32::try_from(word & 0xffff_ffff).unwrap_or(0))
            .unwrap_or(NonZeroU32::MIN);
        let mode = HeapMode::from_raw(u8::try_from((word & MODE_MASK) >> MODE_SHIFT).unwrap_or(0))
            .unwrap_or(HeapMode::Free);
        let leases = u32::try_from((word & LEASE_MASK) >> LEASE_SHIFT).unwrap_or(0);
        Snapshot {
            generation,
            mode,
            retired,
            leases,
        }
    }

    pub(super) fn load(&self) -> Snapshot {
        Self::decode(self.word.load(Ordering::Acquire))
    }

    pub(super) fn store(&self, generation: NonZeroU32, mode: HeapMode, retired: bool, leases: u32) {
        self.word.store(
            Self::pack(generation, mode, retired, leases),
            Ordering::Release,
        );
    }

    pub(super) fn matches(&self, id: HeapId) -> bool {
        let snap = self.load();
        !snap.retired && snap.generation == id.generation()
    }

    pub(crate) fn mode(&self) -> HeapMode {
        self.load().mode
    }

    pub(super) fn generation(&self) -> NonZeroU32 {
        self.load().generation
    }

    pub(super) fn is_retired(&self) -> bool {
        self.load().retired
    }

    pub(super) fn is_free(&self) -> bool {
        let snap = self.load();
        !snap.retired && snap.mode == HeapMode::Free && snap.leases == 0
    }

    pub(crate) fn is_active(&self) -> bool {
        let snap = self.load();
        !snap.retired && snap.mode == HeapMode::Active
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
            if snap.retired || snap.generation != id.generation() || snap.mode != HeapMode::Active {
                return Err(HeapError::InvalidHeap);
            }
            if snap.leases == MAX_LEASES {
                return Err(HeapError::InvalidMetadata);
            }
            let next = Self::pack(snap.generation, snap.mode, false, snap.leases + 1);
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
            if snap.retired || snap.generation != id.generation() {
                return Err(HeapError::InvalidHeap);
            }
            match snap.mode {
                HeapMode::Active => {
                    let next = Self::pack(snap.generation, HeapMode::Draining, false, snap.leases);
                    if self
                        .word
                        .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                HeapMode::Draining => return Ok(()),
                HeapMode::Free => return Err(HeapError::InvalidHeap),
            }
        }
    }

    /// Bump generation and set Free (leases must already be zero), or permanently retire.
    pub(super) fn bump_or_retire(&self) {
        let snap = self.load();
        debug_assert_eq!(snap.mode, HeapMode::Draining);
        debug_assert_eq!(snap.leases, 0);
        match snap
            .generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU32::new)
        {
            Some(next) => self.store(next, HeapMode::Free, false, 0),
            None => self.store(snap.generation, HeapMode::Free, true, 0),
        }
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
