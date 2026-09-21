use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::PageOwner;

#[repr(transparent)]
pub(super) struct AtomicMapEntry {
    raw: AtomicUsize,
}

impl AtomicMapEntry {
    pub(super) fn load(&self) -> MapEntry {
        MapEntry {
            raw: self.raw.load(Ordering::Acquire),
        }
    }

    /// Release store pairs with [`Self::load`]'s Acquire in `get`.
    ///
    /// Caller must hold the `L1Table` write flag for this entry's L2.
    pub(super) fn store(&self, entry: MapEntry) {
        self.raw.store(entry.raw, Ordering::Release);
    }

    pub(super) fn owner(&self) -> Option<PageOwner> {
        let raw = self.raw.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }

        let addr = raw & MapEntry::POINTER_MASK;
        // SAFETY: `from_owner` stores a nonzero exposed address. Arena chunks never
        // unmap; run headers and extent slots are process-immortal.
        let ptr =
            unsafe { NonNull::new_unchecked(core::ptr::with_exposed_provenance_mut::<()>(addr)) };

        if raw & MapEntry::KIND_EXTENT == 0 {
            // SAFETY: the published tag identifies a live `Run` header.
            Some(PageOwner::Run(unsafe { ptr.cast().as_ref() }))
        } else {
            // SAFETY: the published tag identifies an immortal `Extent` slot.
            Some(PageOwner::Extent(unsafe { ptr.cast().as_ref() }))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MapEntry {
    pub(super) raw: usize,
}

impl MapEntry {
    const KIND_EXTENT: usize = 1;
    const POINTER_MASK: usize = !Self::KIND_EXTENT;

    pub(super) const fn empty() -> Self {
        Self { raw: 0 }
    }

    pub(super) fn from_owner(entry: PageOwner) -> Option<Self> {
        let (ptr, kind) = match entry {
            PageOwner::Run(run) => (core::ptr::from_ref(run).expose_provenance(), 0),
            PageOwner::Extent(extent) => (
                core::ptr::from_ref(extent).expose_provenance(),
                Self::KIND_EXTENT,
            ),
        };

        if ptr & Self::KIND_EXTENT != 0 {
            return None;
        }

        Some(Self { raw: ptr | kind })
    }
}
