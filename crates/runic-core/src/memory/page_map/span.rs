use core::{
    num::NonZeroU16,
    sync::atomic::{AtomicU8, AtomicU16, AtomicUsize, Ordering},
};

use super::{
    entry::MapEntry,
    page::{L2Index, L2Segment, PageCount},
};

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SpanRecord {
    first: L2Index,
    pages: PageCount,
    entry: MapEntry,
}

impl SpanRecord {
    pub(super) const fn new(segment: L2Segment, entry: MapEntry) -> Self {
        Self {
            first: segment.first,
            pages: segment.pages,
            entry,
        }
    }

    fn segment(self) -> L2Segment {
        L2Segment {
            first: self.first,
            pages: self.pages,
        }
    }

    pub(super) fn entry(self) -> MapEntry {
        self.entry
    }

    fn contains(self, index: L2Index) -> bool {
        self.segment().contains(index)
    }

    fn overlaps(self, segment: L2Segment) -> bool {
        let own = self.segment().range();
        let other = segment.range();

        own.start < other.end && other.start < own.end
    }

    fn matches(self, segment: L2Segment, entry: MapEntry) -> bool {
        self.segment().range() == segment.range() && self.entry == entry
    }
}

#[repr(C)]
pub(super) struct SpanSlot {
    state: AtomicU8,
    first: AtomicUsize,
    pages: AtomicU16,
    entry: AtomicUsize,
}

impl SpanSlot {
    pub(super) fn is_empty(&self) -> bool {
        self.state.load(Ordering::Acquire) == SpanSlotState::EMPTY
    }

    pub(super) fn set(&self, record: SpanRecord) {
        self.first.store(record.first.get(), Ordering::Relaxed);
        self.pages
            .store(record.pages.value.get(), Ordering::Relaxed);
        self.entry.store(record.entry.raw, Ordering::Relaxed);
        self.state.store(SpanSlotState::OCCUPIED, Ordering::Release);
    }

    pub(super) fn clear(&self) {
        self.state.store(SpanSlotState::EMPTY, Ordering::Release);
    }

    fn record(&self) -> Option<SpanRecord> {
        if self.state.load(Ordering::Acquire) != SpanSlotState::OCCUPIED {
            return None;
        }

        Some(SpanRecord {
            first: L2Index {
                index: self.first.load(Ordering::Relaxed),
            },
            pages: PageCount {
                value: NonZeroU16::new(self.pages.load(Ordering::Relaxed))?,
            },
            entry: MapEntry {
                raw: self.entry.load(Ordering::Relaxed),
            },
        })
    }

    pub(super) fn record_containing(&self, index: L2Index) -> Option<SpanRecord> {
        self.record().filter(|record| record.contains(index))
    }

    pub(super) fn overlaps(&self, segment: L2Segment) -> bool {
        self.record().is_some_and(|record| record.overlaps(segment))
    }

    pub(super) fn covers(&self, segment: L2Segment) -> bool {
        self.record()
            .is_some_and(|record| record.segment().range() == segment.range())
    }

    pub(super) fn matches(&self, segment: L2Segment, entry: MapEntry) -> bool {
        self.record()
            .is_some_and(|record| record.matches(segment, entry))
    }
}

struct SpanSlotState;

impl SpanSlotState {
    const EMPTY: u8 = 0;
    const OCCUPIED: u8 = 1;
}
