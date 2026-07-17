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

    pub(super) fn store(&self, entry: MapEntry) {
        self.raw.store(entry.raw, Ordering::Release);
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
            PageOwner::Run(ptr) => (ptr.cast::<()>().as_ptr().addr(), 0),
            PageOwner::Extent(ptr) => (ptr.cast::<()>().as_ptr().addr(), Self::KIND_EXTENT),
        };

        if ptr & Self::KIND_EXTENT != 0 {
            return None;
        }

        Some(Self { raw: ptr | kind })
    }

    pub(super) const fn is_empty(self) -> bool {
        self.raw == 0
    }

    pub(super) fn owner(self) -> Option<PageOwner> {
        if self.is_empty() {
            return None;
        }

        let raw = self.raw & Self::POINTER_MASK;
        let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut::<()>(raw))?;

        if self.raw & Self::KIND_EXTENT == 0 {
            Some(PageOwner::Run(ptr.cast()))
        } else {
            Some(PageOwner::Extent(ptr.cast()))
        }
    }
}
