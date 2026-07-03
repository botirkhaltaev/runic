use core::ptr::NonNull;

use crate::{
    free_list::FreeList,
    layout::LayoutSpec,
    os_memory::Mapping,
    size_class::{SizeClass, SizeClassId},
};

pub(crate) const SPAN_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SpanId(u32);

impl SpanId {
    pub(crate) const INVALID_RAW: u32 = u32::MAX;

    pub(crate) const fn new(raw: u32) -> Option<Self> {
        if raw == Self::INVALID_RAW {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SpanSlot(u32);

impl SpanSlot {
    pub(crate) const fn empty() -> Self {
        Self(SpanId::INVALID_RAW)
    }

    pub(crate) const fn some(id: SpanId) -> Self {
        Self(id.get())
    }

    pub(crate) const fn get(self) -> Option<SpanId> {
        SpanId::new(self.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockIndex(u32);

#[derive(Clone, Copy)]
pub(crate) struct AddressRange {
    base: NonNull<u8>,
    len: usize,
}

impl AddressRange {
    pub(crate) const fn new(base: NonNull<u8>, len: usize) -> Self {
        Self { base, len }
    }

    pub(crate) const fn base(self) -> NonNull<u8> {
        self.base
    }

    pub(crate) const fn len(self) -> usize {
        self.len
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpanUse {
    Small,
    Large,
}

pub(crate) struct Span {
    id: SpanId,
    base: NonNull<u8>,
    len: usize,
    kind: SpanKind,
}

pub(crate) enum SpanKind {
    Small(SmallSpan),
    Large(LargeSpan),
}

pub(crate) struct SmallSpan {
    class: SizeClassId,
    block_size: usize,
    capacity: u32,
    live: u32,
    free: FreeList,
}

pub(crate) struct LargeSpan {
    user_ptr: NonNull<u8>,
    raw: Mapping,
}

impl Span {
    pub(crate) fn small(id: SpanId, mapping: Mapping, class: SizeClass) -> Self {
        let block_size = class.block_size();
        let capacity = mapping.len() / block_size;
        let mut small = SmallSpan {
            class: class.id(),
            block_size,
            capacity: capacity as u32,
            live: 0,
            free: FreeList::new(),
        };

        for index in 0..capacity {
            let ptr = unsafe { mapping.base().as_ptr().add(index * block_size) };
            let ptr = unsafe { NonNull::new_unchecked(ptr) };
            unsafe { small.free.push(ptr) };
        }

        Self {
            id,
            base: mapping.base(),
            len: mapping.len(),
            kind: SpanKind::Small(small),
        }
    }

    pub(crate) fn large(id: SpanId, mapping: Mapping, spec: LayoutSpec) -> Option<Self> {
        let user_addr = spec.align_addr(mapping.base().as_ptr() as usize)?;
        let user_ptr = NonNull::new(user_addr as *mut u8)?;

        Some(Self {
            id,
            base: user_ptr,
            len: spec.size(),
            kind: SpanKind::Large(LargeSpan {
                user_ptr,
                raw: mapping,
            }),
        })
    }

    pub(crate) const fn id(&self) -> SpanId {
        self.id
    }

    pub(crate) const fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) const fn class_id(&self) -> Option<SizeClassId> {
        match &self.kind {
            SpanKind::Small(small) => Some(small.class),
            SpanKind::Large(_) => None,
        }
    }

    pub(crate) const fn range(&self) -> AddressRange {
        AddressRange::new(self.base, self.len)
    }

    pub(crate) const fn use_kind(&self) -> SpanUse {
        match self.kind {
            SpanKind::Small(_) => SpanUse::Small,
            SpanKind::Large(_) => SpanUse::Large,
        }
    }

    pub(crate) fn take_block(&mut self, spec: LayoutSpec) -> Option<NonNull<u8>> {
        let SpanKind::Small(small) = &mut self.kind else {
            return None;
        };

        let ptr = small.free.pop()?;

        if !(ptr.as_ptr() as usize).is_multiple_of(spec.align()) {
            unsafe { small.free.push(ptr) };
            return None;
        }

        small.live = small.live.checked_add(1)?;
        Some(ptr)
    }

    pub(crate) fn block_index(&self, ptr: NonNull<u8>) -> Option<BlockIndex> {
        let SpanKind::Small(small) = &self.kind else {
            return None;
        };

        small.block_index(self.base, self.len, ptr)
    }

    pub(crate) unsafe fn return_block(&mut self, block: BlockIndex) -> bool {
        let SpanKind::Small(small) = &mut self.kind else {
            return false;
        };

        unsafe { small.return_block(self.base, block) }
    }

    pub(crate) fn raw_mapping_for_large_ptr(&self, ptr: NonNull<u8>) -> Option<Mapping> {
        match &self.kind {
            SpanKind::Large(large) if large.user_ptr == ptr => Some(large.raw),
            _ => None,
        }
    }
}

impl SmallSpan {
    fn block_index(&self, base: NonNull<u8>, len: usize, ptr: NonNull<u8>) -> Option<BlockIndex> {
        let ptr = ptr.as_ptr() as usize;
        let base = base.as_ptr() as usize;
        let end = base.checked_add(len)?;

        if ptr < base || ptr >= end {
            return None;
        }

        let offset = ptr - base;

        if !offset.is_multiple_of(self.block_size) {
            return None;
        }

        let index = offset / self.block_size;

        if index >= self.capacity as usize {
            return None;
        }

        Some(BlockIndex(index as u32))
    }

    unsafe fn return_block(&mut self, base: NonNull<u8>, block: BlockIndex) -> bool {
        let Some(live) = self.live.checked_sub(1) else {
            return false;
        };

        self.live = live;

        let ptr = unsafe { base.as_ptr().add(block.0 as usize * self.block_size) };
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        unsafe { self.free.push(ptr) };
        true
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use crate::{layout::LayoutSpec, os_memory::OsMemory, size_class::SizeClasses};

    use super::*;

    fn class_for(size: usize, align: usize) -> SizeClass {
        let spec = LayoutSpec::from_size_align(size, align).unwrap();
        SizeClasses::new().get(spec).unwrap()
    }

    #[test]
    fn small_span_takes_each_block_once() {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut span = Span::small(SpanId::new(0).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let capacity = SPAN_SIZE / class.block_size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = span.take_block(spec).unwrap();
            let index = span.block_index(ptr).unwrap().0 as usize;

            assert!(!seen[index]);
            assert!(index < capacity);
            assert!((ptr.as_ptr() as usize) >= span.base().as_ptr() as usize);
            assert!((ptr.as_ptr() as usize) < span.base().as_ptr() as usize + SPAN_SIZE);
            seen[index] = true;
        }

        assert!(span.take_block(spec).is_none());
        assert!(seen.into_iter().all(|value| value));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn small_span_reuses_returned_block() {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let class = class_for(128, 8);
        let mut span = Span::small(SpanId::new(1).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(128, 8).unwrap();

        let ptr = span.take_block(spec).unwrap();
        let block = span.block_index(ptr).unwrap();

        assert!(unsafe { span.return_block(block) });

        assert_eq!(span.take_block(spec), Some(ptr));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn small_span_rejects_interior_pointer() {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut span = Span::small(SpanId::new(2).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = span.take_block(spec).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(span.block_index(interior).is_none());

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn small_span_return_block_reports_live_underflow() {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut span = Span::small(SpanId::new(7).unwrap(), mapping, class);
        let block = span.block_index(span.base()).unwrap();

        assert!(!unsafe { span.return_block(block) });

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn small_span_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let memory = OsMemory::new();
        let mapping = memory.map(SPAN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::new().get(spec).unwrap();
        let mut span = Span::small(SpanId::new(3).unwrap(), mapping, class);
        let capacity = SPAN_SIZE / class.block_size();

        for _ in 0..capacity {
            let ptr = span.take_block(spec).unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn large_span_aligns_user_pointer_and_keeps_raw_mapping() {
        let memory = OsMemory::new();
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = memory
            .map(spec.large_mapping_len(memory.page_size()).unwrap())
            .unwrap();
        let span = Span::large(SpanId::new(4).unwrap(), mapping, spec).unwrap();

        assert_eq!(span.base().as_ptr() as usize % spec.align(), 0);
        assert_eq!(span.range().len(), spec.size());
        assert_eq!(span.raw_mapping_for_large_ptr(span.base()), Some(mapping));

        let interior = unsafe { NonNull::new_unchecked(span.base().as_ptr().add(1)) };
        assert_eq!(span.raw_mapping_for_large_ptr(interior), None);

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_use_kind_reports_small_or_large() {
        let memory = OsMemory::new();
        let small_mapping = memory.map(SPAN_SIZE).unwrap();
        let small = Span::small(SpanId::new(5).unwrap(), small_mapping, class_for(8, 8));
        let large_spec =
            LayoutSpec::from_layout(Layout::from_size_align(128 * 1024, 8).unwrap()).unwrap();
        let large_mapping = memory
            .map(large_spec.large_mapping_len(memory.page_size()).unwrap())
            .unwrap();
        let large = Span::large(SpanId::new(6).unwrap(), large_mapping, large_spec).unwrap();

        assert_eq!(small.use_kind(), SpanUse::Small);
        assert_eq!(large.use_kind(), SpanUse::Large);

        unsafe {
            memory.unmap(small_mapping);
            memory.unmap(large_mapping);
        }
    }
}
