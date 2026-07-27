//! Packed heap lifecycle state: generation, mode, retired flag, and publisher leases.

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
const PUBLISHER_SHIFT: u32 = 35;
const PUBLISHER_MASK: u64 = ((1u64 << 29) - 1) << PUBLISHER_SHIFT;
pub(super) const MAX_PUBLISHERS: u32 = (1 << 29) - 1;

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

/// Decoded snapshot of the packed [`SlotState`] word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SlotStateSnapshot {
    pub(super) generation: NonZeroU32,
    pub(super) mode: HeapMode,
    pub(super) retired: bool,
    pub(super) publishers: u32,
}

/// Packed generation + mode + retired + publisher count — sole heap lifecycle authority.
///
/// Linearization / ordering:
/// - Active publish admit: successful `acquire_publisher` `AcqRel` CAS
/// - Inbox publish: head CAS in [`super::inbox::Inbox::link`] (after lease admit)
/// - Active→Draining close: `close_active` `AcqRel` CAS (preserves publisher count)
/// - Publisher release: `Release` `fetch_sub`; retire observes zero with `Acquire` loads
/// - Free reactivation: `Release` store of Active after metadata rebind under the lifecycle lock
pub(crate) struct SlotState {
    word: AtomicU64,
}

impl SlotState {
    pub(super) fn new(generation: NonZeroU32, mode: HeapMode) -> Self {
        Self {
            word: AtomicU64::new(Self::pack(generation, mode, false, 0)),
        }
    }

    fn pack(generation: NonZeroU32, mode: HeapMode, retired: bool, publishers: u32) -> u64 {
        debug_assert!(publishers <= MAX_PUBLISHERS);
        let mut word = u64::from(generation.get());
        word |= u64::from(mode.raw()) << MODE_SHIFT;
        if retired {
            word |= RETIRED_BIT;
        }
        word |= u64::from(publishers) << PUBLISHER_SHIFT;
        word
    }

    fn decode(word: u64) -> SlotStateSnapshot {
        let retired = word & RETIRED_BIT != 0;
        let generation = NonZeroU32::new(u32::try_from(word & 0xffff_ffff).unwrap_or(0))
            .unwrap_or(NonZeroU32::MIN);
        let mode = HeapMode::from_raw(u8::try_from((word & MODE_MASK) >> MODE_SHIFT).unwrap_or(0))
            .unwrap_or(HeapMode::Free);
        let publishers = u32::try_from((word & PUBLISHER_MASK) >> PUBLISHER_SHIFT).unwrap_or(0);
        SlotStateSnapshot {
            generation,
            mode,
            retired,
            publishers,
        }
    }

    pub(super) fn load(&self) -> SlotStateSnapshot {
        Self::decode(self.word.load(Ordering::Acquire))
    }

    pub(super) fn store(
        &self,
        generation: NonZeroU32,
        mode: HeapMode,
        retired: bool,
        publishers: u32,
    ) {
        self.word.store(
            Self::pack(generation, mode, retired, publishers),
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
        !snap.retired && snap.mode == HeapMode::Free && snap.publishers == 0
    }

    pub(crate) fn is_active(&self) -> bool {
        let snap = self.load();
        !snap.retired && snap.mode == HeapMode::Active
    }

    pub(super) fn publishers(&self) -> u32 {
        self.load().publishers
    }

    /// Admit one Active publisher lease for `id`, or fail if closed / overflow.
    ///
    /// Counts in-flight Active **enqueue** admits only — not inbox depth
    /// (that stays live via claim bits / `has_live`). Does not serialize
    /// concurrent freer bodies.
    pub(super) fn acquire_publisher(&self, id: HeapId) -> Result<PublisherLease<'_>, HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.retired || snap.generation != id.generation() || snap.mode != HeapMode::Active {
                return Err(HeapError::InvalidHeap);
            }
            if snap.publishers == MAX_PUBLISHERS {
                return Err(HeapError::InvalidMetadata);
            }
            let next = Self::pack(snap.generation, snap.mode, false, snap.publishers + 1);
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(PublisherLease { state: self });
            }
        }
    }

    fn release_publisher(&self) {
        let amount = 1u64 << PUBLISHER_SHIFT;
        let prev = self.word.fetch_sub(amount, Ordering::Release);
        // Underflow would corrupt mode/generation bits — fail closed.
        if (prev & PUBLISHER_MASK) < amount {
            Allocator::abort();
        }
    }

    /// Close Active admission while preserving the in-flight publisher count.
    pub(super) fn close_active(&self, id: HeapId) -> Result<(), HeapError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let snap = Self::decode(word);
            if snap.retired || snap.generation != id.generation() {
                return Err(HeapError::InvalidHeap);
            }
            match snap.mode {
                HeapMode::Active => {
                    let next =
                        Self::pack(snap.generation, HeapMode::Draining, false, snap.publishers);
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

    /// Bump generation and set Free (publishers must already be zero), or permanently retire.
    pub(super) fn bump_free_or_retire(&self) {
        let snap = self.load();
        debug_assert_eq!(snap.mode, HeapMode::Draining);
        debug_assert_eq!(snap.publishers, 0);
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

/// RAII Active publisher admission. Drop releases the packed count.
///
/// Internal to [`super::slot::HeapSlot::enqueue`] only — taken only when a freer newly
/// queues a run/extent.
#[must_use]
pub(super) struct PublisherLease<'a> {
    state: &'a SlotState,
}

impl Drop for PublisherLease<'_> {
    fn drop(&mut self) {
        self.state.release_publisher();
    }
}
