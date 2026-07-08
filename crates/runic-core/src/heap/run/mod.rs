use core::{num::NonZeroU32, ptr::NonNull};

use spin::Mutex;

mod arena;
mod cache;
mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
    ownership::HeapOwner,
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

    pub(crate) const fn ptr(self) -> NonNull<u8> {
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
enum BlockStateError {
    AlreadyFree,
    InvalidIndex,
}

struct BlockStates {
    reusable: [u64; BLOCK_STATE_WORDS],
}

impl BlockStates {
    fn new(capacity: u32) -> Self {
        let mut reusable = [0; BLOCK_STATE_WORDS];
        let capacity = usize::try_from(capacity).unwrap_or(0).min(MAX_BLOCKS);
        let full_words = capacity / BLOCK_STATE_WORD_BITS;
        let remainder = capacity % BLOCK_STATE_WORD_BITS;

        for word in reusable.iter_mut().take(full_words) {
            *word = u64::MAX;
        }

        if remainder != 0
            && let Some(word) = reusable.get_mut(full_words)
        {
            *word = (1_u64 << remainder) - 1;
        }

        Self { reusable }
    }

    fn take_reusable(&mut self) -> Option<BlockIndex> {
        for word_index in 0..self.reusable.len() {
            let word = self.reusable.get_mut(word_index)?;
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

    fn is_allocated(&self, index: BlockIndex) -> Result<bool, BlockStateError> {
        let Some(reusable) = self.reusable.get(index.word()) else {
            return Err(BlockStateError::InvalidIndex);
        };

        Ok(*reusable & index.mask() == 0)
    }

    fn release(&mut self, index: BlockIndex) -> Result<(), BlockStateError> {
        let Some(reusable) = self.reusable.get_mut(index.word()) else {
            return Err(BlockStateError::InvalidIndex);
        };
        let mask = index.mask();

        if *reusable & mask != 0 {
            return Err(BlockStateError::AlreadyFree);
        }

        *reusable |= mask;
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
    state: Mutex<RunState>,
}

struct RunState {
    owner: HeapOwner,
    live: u32,
    available_next: Option<NonNull<Run>>,
    blocks: BlockStates,
}

pub(crate) struct RunAllocation {
    block: RunBlock,
    has_available_blocks: bool,
}

impl RunAllocation {
    pub(crate) const fn ptr(&self) -> NonNull<u8> {
        self.block.ptr()
    }

    pub(crate) const fn has_available_blocks(&self) -> bool {
        self.has_available_blocks
    }
}

pub(crate) struct RunFreeStatus {
    was_full: bool,
    is_empty: bool,
}

impl RunFreeStatus {
    pub(crate) const fn was_full(&self) -> bool {
        self.was_full
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.is_empty
    }
}

impl Run {
    pub(crate) fn new(id: RunId, owner: HeapOwner, mapping: Mapping, class: SizeClass) -> Self {
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
            state: Mutex::new(RunState {
                owner,
                live: 0,
                available_next: None,
                blocks: BlockStates::new(u32::try_from(capacity).unwrap_or(u32::MAX)),
            }),
        }
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) fn owner(&self) -> HeapOwner {
        self.state.lock().owner
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    pub(crate) fn has_available_blocks(&self) -> bool {
        self.state.lock().live < self.capacity
    }

    pub(crate) fn set_available_next(&self, next: Option<NonNull<Run>>) {
        self.state.lock().available_next = next;
    }

    pub(crate) fn take_available_next(&self) -> Option<NonNull<Run>> {
        self.state.lock().available_next.take()
    }

    pub(crate) fn available_next(&self) -> Option<NonNull<Run>> {
        self.state.lock().available_next
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn into_mapping(self) -> Mapping {
        self.mapping
    }

    pub(crate) fn allocate(&self) -> Option<RunAllocation> {
        let mut state = self.state.lock();
        let index = state.blocks.take_reusable()?;
        let block = RunBlock::at_offset(index, self.range.base(), self.block_size)?;

        state.live = state.live.checked_add(1)?;
        Some(RunAllocation {
            block,
            has_available_blocks: state.live < self.capacity,
        })
    }

    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        let mut state = self.state.lock();
        let was_full = state.live == self.capacity;

        match state.blocks.release(block.index()) {
            Ok(()) => {}
            Err(BlockStateError::AlreadyFree) => return Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => return Err(RunError::InvalidPointer),
        }

        let Some(live) = state.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        state.live = live;

        Ok(RunFreeStatus {
            was_full,
            is_empty: live == 0,
        })
    }

    pub(crate) fn validate_free(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        self.allocated_block_at(ptr)?;
        Ok(())
    }

    pub(crate) fn allocated_block_at(&self, ptr: NonNull<u8>) -> Result<RunBlock, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.state.lock().blocks.is_allocated(block.index()) {
            Ok(true) => Ok(block),
            Ok(false) => Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex | BlockStateError::AlreadyFree) => {
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
    use crate::{
        layout::LayoutSpec, memory::OsMemory, ownership::HeapOwner, size_class::SizeClasses,
    };

    use super::*;

    fn class_for(size: usize, align: usize) -> SizeClass {
        let spec = LayoutSpec::from_size_align(size, align).unwrap();
        SizeClasses::for_layout(spec).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let run = Run::new(
            RunId::from_index(0).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );
        let capacity = RUN_SIZE / class.block_size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let allocation = run.allocate().unwrap();
            let block = allocation.block;
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
        let run = Run::new(
            RunId::from_index(1).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );

        let ptr = run.allocate().unwrap().ptr();

        assert!(run.free(ptr).is_ok());

        assert_eq!(run.allocate().map(|allocation| allocation.ptr()), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            HeapOwner::Shared,
            mapping,
            class_for(64, 8),
        );
        let new = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            HeapOwner::Shared,
            mapping,
            class_for(64, 8),
        );
        let new = LayoutSpec::from_size_align(80, 8).unwrap();
        let ptr = run.allocate().unwrap().ptr();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );
        let ptr = run.allocate().unwrap().ptr();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_rejects_interior_pointer_for_non_power_of_two_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(24, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );
        let ptr = run.allocate().unwrap().ptr();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(ptr).is_some());
        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_reports_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );
        let ptr = run.allocate().unwrap().ptr();

        assert!(run.free(ptr).is_ok());
        assert!(matches!(run.free(ptr), Err(RunError::DoubleFree)));
    }

    #[test]
    fn reusable_run_rejects_never_allocated_block_as_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );

        assert!(matches!(
            run.free(run.range().base()),
            Err(RunError::DoubleFree)
        ));
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::for_layout(spec).unwrap();
        let run = Run::new(
            RunId::from_index(3).unwrap(),
            HeapOwner::Shared,
            mapping,
            class,
        );
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
        let run = Run::new(
            RunId::from_index(5).unwrap(),
            HeapOwner::Shared,
            mapping,
            class_for(8, 8),
        );

        assert_eq!(run.range().base(), range.base());
        assert_eq!(run.range().len(), range.len());
    }
}
