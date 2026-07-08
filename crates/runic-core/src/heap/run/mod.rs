use core::{num::NonZeroU32, ptr::NonNull};

use core::sync::atomic::{AtomicU8, Ordering};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockIndex {
    index: usize,
}

impl BlockIndex {
    const fn new(index: usize) -> Self {
        Self { index }
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

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockState {
    Free = 0,
    Allocated = 1,
}

impl BlockState {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Allocated => 1,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            value if value == Self::Free.raw() => Some(Self::Free),
            value if value == Self::Allocated.raw() => Some(Self::Allocated),
            _ => None,
        }
    }
}

struct BlockStates {
    states: [AtomicU8; MAX_BLOCKS],
}

impl BlockStates {
    fn new() -> Self {
        Self {
            states: [const { AtomicU8::new(BlockState::Free.raw()) }; MAX_BLOCKS],
        }
    }

    fn allocate(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state(index)?;
        debug_assert_eq!(self.load(index)?, BlockState::Free);
        state.store(BlockState::Allocated.raw(), Ordering::Relaxed);
        Ok(())
    }

    fn is_allocated(&self, index: BlockIndex) -> Result<bool, BlockStateError> {
        Ok(self.load(index)? == BlockState::Allocated)
    }

    fn release(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state(index)?;
        match self.load(index)? {
            BlockState::Allocated => {
                state.store(BlockState::Free.raw(), Ordering::Relaxed);
                Ok(())
            }
            BlockState::Free => Err(BlockStateError::AlreadyFree),
        }
    }

    fn load(&self, index: BlockIndex) -> Result<BlockState, BlockStateError> {
        let raw = self.state(index)?.load(Ordering::Relaxed);
        BlockState::from_raw(raw).ok_or(BlockStateError::InvalidIndex)
    }

    fn state(&self, index: BlockIndex) -> Result<&AtomicU8, BlockStateError> {
        self.states
            .get(index.index)
            .ok_or(BlockStateError::InvalidIndex)
    }
}

pub(crate) struct Run {
    id: RunId,
    mapping: Mapping,
    range: AddressRange,
    class: SizeClassId,
    block_size: usize,
    block_shift: Option<u32>,
    capacity: usize,
    state: std::cell::UnsafeCell<RunState>,
    blocks: BlockStates,
}

// SAFETY: owner-local methods are externally synchronized by the owning heap in the current
// architecture. Remote methods only touch atomic block state.
unsafe impl Sync for Run {}

struct RunState {
    owner: HeapOwner,
    live: usize,
    bump: usize,
    available_next: Option<NonNull<Run>>,
    free: Option<NonNull<u8>>,
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
            capacity: capacity.min(MAX_BLOCKS),
            state: std::cell::UnsafeCell::new(RunState {
                owner,
                live: 0,
                bump: 0,
                available_next: None,
                free: None,
            }),
            blocks: BlockStates::new(),
        }
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) fn owner(&self) -> HeapOwner {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        unsafe { &*self.state.get() }.owner
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    pub(crate) fn has_available_blocks(&self) -> bool {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        unsafe { &*self.state.get() }.live < self.capacity
    }

    pub(crate) fn set_available_next(&self, next: Option<NonNull<Run>>) {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        unsafe { &mut *self.state.get() }.available_next = next;
    }

    pub(crate) fn take_available_next(&self) -> Option<NonNull<Run>> {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        unsafe { &mut *self.state.get() }.available_next.take()
    }

    pub(crate) fn available_next(&self) -> Option<NonNull<Run>> {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        unsafe { &*self.state.get() }.available_next
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn into_mapping(self) -> Mapping {
        self.mapping
    }

    pub(crate) fn allocate(&self) -> Option<RunAllocation> {
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let (index, ptr) = if let Some(ptr) = state.pop_free() {
            let block = self.block_at(ptr)?;
            (block.index(), ptr)
        } else {
            let index = state.allocate_fresh(self.capacity)?;
            (index, self.block_ptr(index)?)
        };
        let block = RunBlock::new(index, ptr);

        self.blocks.allocate(index).ok()?;

        state.live = state.live.checked_add(1)?;
        Some(RunAllocation {
            block,
            has_available_blocks: state.live < self.capacity,
        })
    }

    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are externally synchronized by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let was_full = state.live == self.capacity;

        match self.blocks.release(block.index()) {
            Ok(()) => {}
            Err(BlockStateError::AlreadyFree) => return Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => return Err(RunError::InvalidPointer),
        }

        let Some(live) = state.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        state.live = live;
        state.push_free(ptr);

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

        match self.blocks.is_allocated(block.index()) {
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
        if index >= self.capacity {
            return None;
        }

        Some(RunBlock::new(BlockIndex::new(index), ptr))
    }

    fn block_ptr(&self, index: BlockIndex) -> Option<NonNull<u8>> {
        if index.index >= self.capacity {
            return None;
        }

        RunBlock::at_offset(index, self.range.base(), self.block_size).map(RunBlock::ptr)
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

impl RunState {
    fn allocate_fresh(&mut self, capacity: usize) -> Option<BlockIndex> {
        if self.bump >= capacity {
            return None;
        }

        let index = BlockIndex::new(self.bump);
        self.bump += 1;
        Some(index)
    }

    fn pop_free(&mut self) -> Option<NonNull<u8>> {
        let ptr = self.free?;
        self.free = Self::read_next(ptr);
        Some(ptr)
    }

    fn push_free(&mut self, ptr: NonNull<u8>) {
        Self::write_next(ptr, self.free);
        self.free = Some(ptr);
    }

    fn read_next(ptr: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        NonNull::new(unsafe { ptr.cast::<*mut u8>().as_ptr().read() })
    }

    fn write_next(ptr: NonNull<u8>, next: Option<NonNull<u8>>) {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        unsafe {
            ptr.cast::<*mut u8>()
                .as_ptr()
                .write(next.map_or(core::ptr::null_mut(), NonNull::as_ptr));
        }
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
