use core::{num::NonZeroU32, ptr::NonNull};

mod arena;
mod cache;
mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
    size_class::{SizeClass, SizeClassId},
};

pub(crate) use arena::{RunArena, RunReservation};
pub(crate) use heap::{RunHeap, RunHeapError};

pub(crate) const RUN_SIZE: usize = 64 * 1024;
const MIN_BLOCK_SIZE: usize = 8;
const MAX_BLOCKS: usize = RUN_SIZE / MIN_BLOCK_SIZE;
const BLOCK_STATE_WORD_BITS: usize = 64;
const BLOCK_STATE_WORDS: usize = MAX_BLOCKS.div_ceil(BLOCK_STATE_WORD_BITS);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunId {
    index: NonZeroU32,
}

impl RunId {
    pub(crate) fn from_index(index: u32) -> Option<Self> {
        NonZeroU32::new(index.checked_add(1)?).map(|index| Self { index })
    }

    pub(crate) const fn index(self) -> u32 {
        self.index.get() - 1
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockIndex {
    index: usize,
}

impl BlockIndex {
    const fn new(index: usize) -> Self {
        Self { index }
    }

    fn word(self) -> usize {
        self.index / BLOCK_STATE_WORD_BITS
    }

    fn mask(self) -> u64 {
        1_u64 << (self.index % BLOCK_STATE_WORD_BITS)
    }

    fn offset(self, block_size: usize) -> Option<usize> {
        self.index.checked_mul(block_size)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RunBlock {
    index: BlockIndex,
    ptr: NonNull<u8>,
}

impl RunBlock {
    const fn new(index: BlockIndex, ptr: NonNull<u8>) -> Self {
        Self { index, ptr }
    }

    const fn index(self) -> BlockIndex {
        self.index
    }

    const fn ptr(self) -> NonNull<u8> {
        self.ptr
    }

    fn at_offset(index: BlockIndex, base: NonNull<u8>, block_size: usize) -> Option<Self> {
        let offset = index.offset(block_size)?;
        // SAFETY: caller constructs indexes from this run's capacity, so offset is in range.
        let ptr = unsafe { NonNull::new_unchecked(base.as_ptr().add(offset)) };

        Some(Self::new(index, ptr))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    InvalidPointer,
    DoubleFree,
    FreeUnderflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreeBitmapError {
    AlreadyFree,
    InvalidIndex,
}

struct FreeBitmap {
    words: [u64; BLOCK_STATE_WORDS],
}

impl FreeBitmap {
    fn new(capacity: u32) -> Self {
        let mut words = [0; BLOCK_STATE_WORDS];
        let capacity = usize::try_from(capacity).unwrap_or(0).min(MAX_BLOCKS);
        let full_words = capacity / BLOCK_STATE_WORD_BITS;
        let remainder = capacity % BLOCK_STATE_WORD_BITS;

        for word in words.iter_mut().take(full_words) {
            *word = u64::MAX;
        }

        if remainder != 0
            && let Some(word) = words.get_mut(full_words)
        {
            *word = (1_u64 << remainder) - 1;
        }

        Self { words }
    }

    fn take_free(&mut self) -> Option<BlockIndex> {
        for word_index in 0..self.words.len() {
            let word = self.words.get_mut(word_index)?;
            if *word == 0 {
                continue;
            }

            let bit = word.trailing_zeros();
            *word &= !(1_u64 << bit);
            let index = word_index
                .checked_mul(BLOCK_STATE_WORD_BITS)?
                .checked_add(usize::try_from(bit).ok()?)?;

            return Some(BlockIndex::new(index));
        }

        None
    }

    fn is_allocated(&self, index: BlockIndex) -> Result<bool, FreeBitmapError> {
        let Some(word) = self.words.get(index.word()) else {
            return Err(FreeBitmapError::InvalidIndex);
        };

        Ok(*word & index.mask() == 0)
    }

    fn release(&mut self, index: BlockIndex) -> Result<(), FreeBitmapError> {
        let Some(word) = self.words.get_mut(index.word()) else {
            return Err(FreeBitmapError::InvalidIndex);
        };
        let mask = index.mask();

        if *word & mask != 0 {
            return Err(FreeBitmapError::AlreadyFree);
        }

        *word |= mask;
        Ok(())
    }
}

pub(crate) struct Run {
    id: RunId,
    mapping: Mapping,
    range: AddressRange,
    class: SizeClassId,
    block_size: usize,
    block_shift: Option<u32>,
    capacity: u32,
    live: u32,
    available_next: Option<NonNull<Run>>,
    free: FreeBitmap,
}

impl Run {
    pub(crate) fn new(id: RunId, mapping: Mapping, class: SizeClass) -> Self {
        let range = mapping.range();
        let block_size = class.block_size();
        let capacity = range.len().checked_div(block_size).unwrap_or(0);
        Self {
            id,
            mapping,
            range,
            class: class.id(),
            block_size,
            block_shift: block_size_shift(block_size),
            capacity: u32::try_from(capacity).unwrap_or(u32::MAX),
            live: 0,
            available_next: None,
            free: FreeBitmap::new(u32::try_from(capacity).unwrap_or(u32::MAX)),
        }
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    pub(crate) const fn has_available_blocks(&self) -> bool {
        self.live < self.capacity
    }

    pub(crate) const fn is_full(&self) -> bool {
        !self.has_available_blocks()
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub(crate) fn set_available_next(&mut self, next: Option<NonNull<Run>>) {
        self.available_next = next;
    }

    pub(crate) fn take_available_next(&mut self) -> Option<NonNull<Run>> {
        self.available_next.take()
    }

    pub(crate) const fn available_next(&self) -> Option<NonNull<Run>> {
        self.available_next
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn into_mapping(self) -> Mapping {
        self.mapping
    }

    pub(crate) fn allocate(&mut self) -> Option<RunBlock> {
        let index = self.free.take_free()?;
        let block = RunBlock::at_offset(index, self.range.base(), self.block_size)?;

        self.live = self.live.checked_add(1)?;
        Some(block)
    }

    pub(crate) fn free(&mut self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        self.return_block(block)
    }

    pub(crate) fn allocated_block_at(&self, ptr: NonNull<u8>) -> Result<RunBlock, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.free.is_allocated(block.index()) {
            Ok(true) => Ok(block),
            Ok(false) => Err(RunError::DoubleFree),
            Err(FreeBitmapError::InvalidIndex | FreeBitmapError::AlreadyFree) => {
                Err(RunError::InvalidPointer)
            }
        }
    }

    pub(crate) fn resize_in_place(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, RunError> {
        self.allocated_block_at(ptr)?;

        Ok(self.block_size >= spec.size() && spec.is_addr_aligned(ptr.as_ptr().addr()))
    }

    pub(crate) fn block_at(&self, ptr: NonNull<u8>) -> Option<RunBlock> {
        let offset = self.range.offset_of(ptr)?;
        let index = self.block_index(offset)?;
        let capacity = usize::try_from(self.capacity).ok()?;

        if index >= capacity {
            return None;
        }

        Some(RunBlock::new(BlockIndex::new(index), ptr))
    }

    fn block_index(&self, offset: usize) -> Option<usize> {
        if let Some(shift) = self.block_shift {
            if offset & (self.block_size - 1) != 0 {
                return None;
            }

            return Some(offset >> shift);
        }

        if !offset.is_multiple_of(self.block_size) {
            return None;
        }

        offset.checked_div(self.block_size)
    }

    fn return_block(&mut self, block: RunBlock) -> Result<(), RunError> {
        match self.free.release(block.index()) {
            Ok(()) => {}
            Err(FreeBitmapError::AlreadyFree) => return Err(RunError::DoubleFree),
            Err(FreeBitmapError::InvalidIndex) => return Err(RunError::InvalidPointer),
        }

        let Some(live) = self.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        self.live = live;

        Ok(())
    }
}

const fn block_size_shift(block_size: usize) -> Option<u32> {
    if block_size.is_power_of_two() {
        Some(block_size.trailing_zeros())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::{layout::LayoutSpec, memory::OsMemory, size_class::SizeClasses};

    use super::*;

    fn class_for(size: usize, align: usize) -> SizeClass {
        let spec = LayoutSpec::from_size_align(size, align).unwrap();
        SizeClasses::for_layout(spec).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(0).unwrap(), mapping, class);
        let capacity = RUN_SIZE / class.block_size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let block = run.allocate().unwrap();
            let ptr = block.ptr();
            let index = block.index().index;

            assert!(!seen[index]);
            assert!(index < capacity);
            assert!((ptr.as_ptr() as usize) >= run.range().base().as_ptr() as usize);
            assert!((ptr.as_ptr() as usize) < run.range().base().as_ptr() as usize + RUN_SIZE);
            seen[index] = true;
        }

        assert!(run.allocate().is_none());
        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn reusable_run_reuses_returned_block() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(128, 8);
        let mut run = Run::new(RunId::from_index(1).unwrap(), mapping, class);

        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.free(ptr), Ok(()));

        assert_eq!(run.allocate().map(RunBlock::ptr), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let mut run = Run::new(RunId::from_index(7).unwrap(), mapping, class_for(64, 8));
        let new = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let mut run = Run::new(RunId::from_index(8).unwrap(), mapping, class_for(64, 8));
        let new = LayoutSpec::from_size_align(80, 8).unwrap();
        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(2).unwrap(), mapping, class);
        let ptr = run.allocate().unwrap().ptr();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_rejects_interior_pointer_for_non_power_of_two_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(24, 8);
        let mut run = Run::new(RunId::from_index(2).unwrap(), mapping, class);
        let ptr = run.allocate().unwrap().ptr();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(ptr).is_some());
        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_reports_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(7).unwrap(), mapping, class);
        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.free(ptr), Ok(()));
        assert_eq!(run.free(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn reusable_run_rejects_never_allocated_block_as_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(8).unwrap(), mapping, class);

        assert_eq!(run.free(run.range().base()), Err(RunError::DoubleFree));
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let mut run = Run::new(RunId::from_index(3).unwrap(), mapping, class);
        let capacity = RUN_SIZE / class.block_size();

        for _ in 0..capacity {
            let ptr = run.allocate().unwrap().ptr();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn run_range_reports_mapping_range() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let range = mapping.range();
        let run = Run::new(RunId::from_index(5).unwrap(), mapping, class_for(8, 8));

        assert_eq!(run.range().base(), range.base());
        assert_eq!(run.range().len(), range.len());
    }
}
