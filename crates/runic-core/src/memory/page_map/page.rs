use core::{num::NonZeroU16, ops::Range, ptr::NonNull};

use super::{ADDRESSABLE_PAGES, L1_ENTRIES, L2_BITS, L2_ENTRIES, PAGE_SHIFT};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PageRange {
    first: Page,
    end: Page,
}

impl PageRange {
    pub(super) fn new(base: NonNull<u8>, len: usize) -> Option<Self> {
        let first = Page::containing(base);
        let end_addr = base.as_ptr().addr().checked_add(len.checked_sub(1)?)?;
        let end = Page {
            number: (end_addr >> PAGE_SHIFT).checked_add(1)?,
        };

        if first.number >= ADDRESSABLE_PAGES || end.number > ADDRESSABLE_PAGES {
            return None;
        }

        Some(Self { first, end })
    }

    pub(super) fn segments(self) -> PageSegments {
        PageSegments {
            next_page: self.first.number,
            end_page: self.end.number,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Page {
    number: usize,
}

impl Page {
    pub(super) fn containing(ptr: NonNull<u8>) -> Self {
        Self {
            number: ptr.as_ptr().addr() >> PAGE_SHIFT,
        }
    }

    pub(super) const fn indexes(self) -> Option<(L1Index, L2Index)> {
        let l2 = self.number & (L2_ENTRIES - 1);
        let l1 = self.number >> L2_BITS;

        if l1 >= L1_ENTRIES {
            return None;
        }

        Some((L1Index { index: l1 }, L2Index { index: l2 }))
    }
}

#[derive(Clone, Copy)]
pub(super) struct PageSegment {
    pub(super) l1: L1Index,
    pub(super) l2: L2Segment,
}

#[derive(Clone, Copy)]
pub(super) struct L2Segment {
    pub(super) first: L2Index,
    pub(super) pages: PageCount,
}

impl L2Segment {
    pub(super) fn new(first: L2Index, pages: usize) -> Option<Self> {
        let pages = PageCount::new(pages)?;
        let end = first.get().checked_add(pages.get())?;

        if end > L2_ENTRIES {
            return None;
        }

        Some(Self { first, pages })
    }

    pub(super) fn range(self) -> Range<usize> {
        let start = self.first.get();
        let end = start + self.pages.get();

        start..end
    }

    pub(super) fn contains(self, index: L2Index) -> bool {
        self.range().contains(&index.get())
    }

    pub(super) fn pages(self) -> u32 {
        self.pages.get_u32()
    }
}

#[derive(Clone, Copy)]
pub(super) struct PageCount {
    pub(super) value: NonZeroU16,
}

impl PageCount {
    pub(super) fn new(pages: usize) -> Option<Self> {
        let pages = u16::try_from(pages).ok()?;
        NonZeroU16::new(pages).map(|value| Self { value })
    }

    pub(super) fn get(self) -> usize {
        usize::from(self.value.get())
    }

    pub(super) fn get_u32(self) -> u32 {
        u32::from(self.value.get())
    }
}

pub(super) struct PageSegments {
    next_page: usize,
    end_page: usize,
}

impl Iterator for PageSegments {
    type Item = PageSegment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_page >= self.end_page {
            return None;
        }

        let l2 = self.next_page & (L2_ENTRIES - 1);
        let l1 = self.next_page >> L2_BITS;
        if l1 >= L1_ENTRIES {
            return None;
        }

        let remaining = self.end_page - self.next_page;
        let pages = remaining.min(L2_ENTRIES - l2);
        let next_page = self.next_page.checked_add(pages)?;
        let l2 = L2Segment::new(L2Index { index: l2 }, pages)?;
        self.next_page = next_page;

        Some(PageSegment {
            l1: L1Index { index: l1 },
            l2,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct L1Index {
    pub(super) index: usize,
}

impl L1Index {
    pub(super) const fn get(self) -> usize {
        self.index
    }
}

#[derive(Clone, Copy)]
pub(super) struct L2Index {
    pub(super) index: usize,
}

impl L2Index {
    pub(super) const fn get(self) -> usize {
        self.index
    }
}
