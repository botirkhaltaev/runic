use core::{num::NonZeroU32, ptr::NonNull};

mod allocator;
mod free_list;
mod table;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
    size_class::{SizeClass, SizeClassId},
};

use free_list::{FreeBlock, FreeList};

pub(crate) use allocator::{RunAllocator, RunAllocatorError};
pub(crate) use table::{RunReservation, RunTable};

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
    index: u32,
}

impl BlockIndex {
    const fn new(index: u32) -> Self {
        Self { index }
    }

    fn word(self) -> Option<usize> {
        usize::try_from(self.index)
            .ok()
            .map(|index| index / BLOCK_STATE_WORD_BITS)
    }

    fn mask(self) -> u64 {
        1_u64 << (self.index % u64::BITS)
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

    unsafe fn free_block(self) -> FreeBlock {
        // SAFETY: caller guarantees this run block is currently free-list eligible.
        unsafe { FreeBlock::new_unchecked(self.ptr) }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AllocatedBlock {
    block: RunBlock,
}

impl AllocatedBlock {
    const fn new(block: RunBlock) -> Self {
        Self { block }
    }

    const fn block(self) -> RunBlock {
        self.block
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    InvalidPointer,
    DoubleFree,
    FreeUnderflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockStateError {
    AlreadyAllocated,
    AlreadyFree,
    InvalidIndex,
}

struct BlockStates {
    words: [u64; BLOCK_STATE_WORDS],
}

impl BlockStates {
    const fn new() -> Self {
        Self {
            words: [0; BLOCK_STATE_WORDS],
        }
    }

    fn mark_allocated(&mut self, index: BlockIndex) -> Result<(), BlockStateError> {
        let word = self.word_mut(index)?;
        let mask = index.mask();

        if *word & mask != 0 {
            return Err(BlockStateError::AlreadyAllocated);
        }

        *word |= mask;
        Ok(())
    }

    fn mark_free(&mut self, index: BlockIndex) -> Result<(), BlockStateError> {
        let word = self.word_mut(index)?;
        let mask = index.mask();

        if *word & mask == 0 {
            return Err(BlockStateError::AlreadyFree);
        }

        *word &= !mask;
        Ok(())
    }

    fn is_allocated(&self, index: BlockIndex) -> Result<bool, BlockStateError> {
        let Some(word) = index.word().and_then(|word| self.words.get(word)) else {
            return Err(BlockStateError::InvalidIndex);
        };

        Ok(*word & index.mask() != 0)
    }

    fn word_mut(&mut self, index: BlockIndex) -> Result<&mut u64, BlockStateError> {
        let Some(word) = index.word().and_then(|word| self.words.get_mut(word)) else {
            return Err(BlockStateError::InvalidIndex);
        };

        Ok(word)
    }
}

pub(crate) struct Run {
    id: RunId,
    mapping: Mapping,
    range: AddressRange,
    class: SizeClassId,
    block_size: usize,
    capacity: u32,
    live: u32,
    states: BlockStates,
    free: FreeList,
}

impl Run {
    pub(crate) fn new(id: RunId, mapping: Mapping, class: SizeClass) -> Self {
        let range = mapping.range();
        let block_size = class.block_size();
        let capacity = range.len().checked_div(block_size).unwrap_or(0);
        let mut run = Self {
            id,
            mapping,
            range,
            class: class.id(),
            block_size,
            capacity: u32::try_from(capacity).unwrap_or(u32::MAX),
            live: 0,
            states: BlockStates::new(),
            free: FreeList::new(),
        };

        for index in 0..capacity {
            let Some(offset) = index.checked_mul(block_size) else {
                break;
            };
            // SAFETY: offset is within the run because index < range.len() / block_size.
            let raw_ptr = unsafe { range.base().as_ptr().add(offset) };
            // SAFETY: raw_ptr is derived from a non-null mapping base plus an in-bounds offset.
            let block_ptr = unsafe { NonNull::new_unchecked(raw_ptr) };
            let Some(block_index) = u32::try_from(index).ok().map(BlockIndex::new) else {
                break;
            };
            let block = RunBlock::new(block_index, block_ptr);
            // SAFETY: newly created run blocks start free and are valid free-list storage.
            unsafe { run.free.push(block.free_block()) };
        }

        run
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn allocate(&mut self, spec: LayoutSpec) -> Option<NonNull<u8>> {
        let free = self.free.pop()?;
        let block = self.block_at(free.ptr())?;

        if !block.ptr().as_ptr().addr().is_multiple_of(spec.align()) {
            // SAFETY: block was just popped from this run's free list and is being restored unchanged.
            unsafe { self.free.push(block.free_block()) };
            return None;
        }

        self.states.mark_allocated(block.index()).ok()?;
        self.live = self.live.checked_add(1)?;
        Some(block.ptr())
    }

    pub(crate) fn free(&mut self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.allocated_block_at(ptr)?;

        self.return_allocated_block(block)
    }

    pub(crate) fn allocated_block_at(&self, ptr: NonNull<u8>) -> Result<AllocatedBlock, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.states.is_allocated(block.index()) {
            Ok(true) => Ok(AllocatedBlock::new(block)),
            Ok(false) => Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => Err(RunError::InvalidPointer),
            Err(BlockStateError::AlreadyAllocated | BlockStateError::AlreadyFree) => {
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

        Ok(self.block_size >= spec.size() && ptr.as_ptr().addr().is_multiple_of(spec.align()))
    }

    pub(crate) fn block_at(&self, ptr: NonNull<u8>) -> Option<RunBlock> {
        let offset = self.range.offset_of(ptr)?;

        if !offset.is_multiple_of(self.block_size) {
            return None;
        }

        let index = offset.checked_div(self.block_size)?;
        let capacity = usize::try_from(self.capacity).ok()?;

        if index >= capacity {
            return None;
        }

        Some(RunBlock::new(
            BlockIndex::new(u32::try_from(index).ok()?),
            ptr,
        ))
    }

    fn return_allocated_block(&mut self, allocated: AllocatedBlock) -> Result<(), RunError> {
        let block = allocated.block();

        match self.states.mark_free(block.index()) {
            Ok(()) => {}
            Err(BlockStateError::AlreadyFree) => return Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex | BlockStateError::AlreadyAllocated) => {
                return Err(RunError::InvalidPointer);
            }
        }

        let Some(live) = self.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        self.live = live;

        // SAFETY: state transition above made this validated block free-list eligible.
        unsafe { self.free.push(block.free_block()) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{layout::LayoutSpec, memory::OsMemory, size_class::SizeClasses};

    use super::*;

    fn class_for(size: usize, align: usize) -> SizeClass {
        let spec = LayoutSpec::from_size_align(size, align).unwrap();
        SizeClasses::get(spec).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(0).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let capacity = RUN_SIZE / class.block_size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = run.allocate(spec).unwrap();
            let index = usize::try_from(run.block_at(ptr).unwrap().index().index).unwrap();

            assert!(!seen[index]);
            assert!(index < capacity);
            assert!((ptr.as_ptr() as usize) >= run.range().base().as_ptr() as usize);
            assert!((ptr.as_ptr() as usize) < run.range().base().as_ptr() as usize + RUN_SIZE);
            seen[index] = true;
        }

        assert!(run.allocate(spec).is_none());
        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn reusable_run_reuses_returned_block() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(128, 8);
        let mut run = Run::new(RunId::from_index(1).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(128, 8).unwrap();

        let ptr = run.allocate(spec).unwrap();
        let block = run.allocated_block_at(ptr).unwrap();

        assert_eq!(run.return_allocated_block(block), Ok(()));

        assert_eq!(run.allocate(spec), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let mut run = Run::new(RunId::from_index(7).unwrap(), mapping, class_for(64, 8));
        let old = LayoutSpec::from_size_align(48, 8).unwrap();
        let new = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate(old).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let mut run = Run::new(RunId::from_index(8).unwrap(), mapping, class_for(64, 8));
        let old = LayoutSpec::from_size_align(64, 8).unwrap();
        let new = LayoutSpec::from_size_align(80, 8).unwrap();
        let ptr = run.allocate(old).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(2).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate(spec).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_reports_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::from_index(7).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate(spec).unwrap();

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
        let class = SizeClasses::get(spec).unwrap();
        let mut run = Run::new(RunId::from_index(3).unwrap(), mapping, class);
        let capacity = RUN_SIZE / class.block_size();

        for _ in 0..capacity {
            let ptr = run.allocate(spec).unwrap();
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
