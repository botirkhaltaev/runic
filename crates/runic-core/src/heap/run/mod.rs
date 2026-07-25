use core::{cell::UnsafeCell, mem::size_of, num::NonZeroU32, ptr::NonNull};

use core::sync::atomic::{AtomicU8, Ordering};

pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping, OsMemory},
    size_class::{SizeClassId, SizeClasses},
};

use super::HeapId;

pub(crate) use heap::{RunHeap, RunHeapError};

pub(crate) const RUN_SIZE: usize = 64 * 1024;

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
    AlreadyAllocated,
    AlreadyPending,
    InvalidIndex,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockState {
    Free = 0,
    Allocated = 1,
    RemotePending = 2,
}

impl BlockState {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Allocated => 1,
            Self::RemotePending => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            value if value == Self::Free.raw() => Some(Self::Free),
            value if value == Self::Allocated.raw() => Some(Self::Allocated),
            value if value == Self::RemotePending.raw() => Some(Self::RemotePending),
            _ => None,
        }
    }
}

/// Per-block Free / Allocated / `RemotePending` state.
///
/// One `AtomicU8` per block for this run's capacity, owned in a dedicated
/// anonymous mapping (zero-filled ⇒ Free). Not a packed bitmap; this is the
/// only free/allocated/remote-pending tracker on a run.
struct BlockStates {
    mapping: Mapping,
    len: usize,
}

impl BlockStates {
    fn new(capacity: usize) -> Option<Self> {
        if capacity == 0 {
            return None;
        }

        let mapping = OsMemory::map(capacity)?;
        Some(Self {
            mapping,
            len: capacity,
        })
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
            BlockState::RemotePending => Err(BlockStateError::AlreadyPending),
        }
    }

    fn mark_remote_pending(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state(index)?;
        match state.compare_exchange(
            BlockState::Allocated.raw(),
            BlockState::RemotePending.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(observed) => Self::state_error(observed),
        }
    }

    fn release_remote_pending(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state(index)?;
        match state.compare_exchange(
            BlockState::RemotePending.raw(),
            BlockState::Free.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(observed) => Self::state_error(observed),
        }
    }

    fn unclaim_remote_pending(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state(index)?;
        match state.compare_exchange(
            BlockState::RemotePending.raw(),
            BlockState::Allocated.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(observed) => Self::state_error(observed),
        }
    }

    fn load(&self, index: BlockIndex) -> Result<BlockState, BlockStateError> {
        let raw = self.state(index)?.load(Ordering::Relaxed);
        BlockState::from_raw(raw).ok_or(BlockStateError::InvalidIndex)
    }

    fn state_error(raw: u8) -> Result<(), BlockStateError> {
        match BlockState::from_raw(raw) {
            Some(BlockState::Free) => Err(BlockStateError::AlreadyFree),
            Some(BlockState::Allocated) => Err(BlockStateError::AlreadyAllocated),
            Some(BlockState::RemotePending) => Err(BlockStateError::AlreadyPending),
            None => Err(BlockStateError::InvalidIndex),
        }
    }

    fn state(&self, index: BlockIndex) -> Result<&AtomicU8, BlockStateError> {
        if index.index >= self.len {
            return Err(BlockStateError::InvalidIndex);
        }

        // SAFETY: `index` is in `0..len`. The mapping is uniquely owned by this
        // `BlockStates`, sized for at least `len` bytes, zero-filled as Free, and
        // shared only through `Run`'s owner-local / atomic remote protocols.
        Ok(unsafe {
            let ptr = self
                .mapping
                .base()
                .as_ptr()
                .add(index.index)
                .cast::<AtomicU8>();
            &*ptr
        })
    }
}

pub(crate) struct Run {
    id: RunId,
    heap: HeapId,
    mapping: Mapping,
    class: SizeClassId,
    block_size: usize,
    block_shift: Option<u32>,
    capacity: usize,
    state: UnsafeCell<RunState>,
    blocks: BlockStates,
}

// SAFETY: owner-local methods are called only by the owning heap. Remote methods only touch atomic
// block state and never mutate RunState.
unsafe impl Sync for Run {}

struct RunState {
    live: usize,
    bump: usize,
    available_next: Option<NonNull<Run>>,
    free: Option<NonNull<u8>>,
}

pub(crate) struct RunFreeStatus {
    was_full: bool,
}

impl RunFreeStatus {
    pub(crate) const fn was_full(&self) -> bool {
        self.was_full
    }
}

impl Run {
    pub(crate) fn new(
        id: RunId,
        heap: HeapId,
        mapping: Mapping,
        class: SizeClassId,
    ) -> Option<Self> {
        let block_size = SizeClasses::block_size(class);
        let capacity = mapping.range().len().checked_div(block_size).unwrap_or(0);
        let blocks = BlockStates::new(capacity)?;
        Some(Self {
            id,
            heap,
            mapping,
            class,
            block_size,
            block_shift: block_size_shift(block_size),
            capacity,
            state: UnsafeCell::new(RunState::new(block_size)),
            blocks,
        })
    }

    #[cfg(test)]
    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) fn set_heap_id(&mut self, heap: HeapId) {
        self.heap = heap;
    }

    pub(crate) const fn heap_id(&self) -> HeapId {
        self.heap
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    pub(crate) fn has_available_blocks(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &*self.state.get() }.live < self.capacity
    }

    pub(crate) fn set_available_next(&self, next: Option<NonNull<Run>>) {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &mut *self.state.get() }.available_next = next;
    }

    pub(crate) fn take_available_next(&self) -> Option<NonNull<Run>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &mut *self.state.get() }.available_next.take()
    }

    pub(crate) fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    pub(crate) fn range(&self) -> AddressRange {
        self.mapping.range()
    }

    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let (index, ptr) = if let Some(ptr) = state.pop_free() {
            let block = self.block_at(ptr)?;
            (block.index(), ptr)
        } else {
            let index = state.allocate_fresh(self.capacity)?;
            (index, self.block_ptr(index)?)
        };
        debug_assert_eq!(self.block_at(ptr).map(RunBlock::index), Some(index));
        self.blocks.allocate(index).ok()?;

        debug_assert!(state.live < self.capacity);
        state.live += 1;
        Some(ptr)
    }

    pub(crate) fn free_local(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let was_full = state.live == self.capacity;

        match self.blocks.release(block.index()) {
            Ok(()) => {}
            Err(
                BlockStateError::AlreadyFree
                | BlockStateError::AlreadyAllocated
                | BlockStateError::AlreadyPending,
            ) => return Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => return Err(RunError::InvalidPointer),
        }

        let Some(live) = state.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        state.live = live;
        state.push_free(ptr);

        Ok(RunFreeStatus { was_full })
    }

    pub(crate) fn claim_free(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.blocks.mark_remote_pending(block.index()) {
            Ok(()) => Ok(()),
            Err(
                BlockStateError::AlreadyFree
                | BlockStateError::AlreadyAllocated
                | BlockStateError::AlreadyPending,
            ) => Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => Err(RunError::InvalidPointer),
        }
    }

    pub(crate) fn unclaim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.blocks.unclaim_remote_pending(block.index()) {
            Ok(()) => Ok(()),
            Err(
                BlockStateError::AlreadyFree
                | BlockStateError::AlreadyAllocated
                | BlockStateError::AlreadyPending,
            ) => Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => Err(RunError::InvalidPointer),
        }
    }

    pub(crate) fn complete_remote_free(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let was_full = state.live == self.capacity;

        match self.blocks.release_remote_pending(block.index()) {
            Ok(()) => {}
            Err(
                BlockStateError::AlreadyFree
                | BlockStateError::AlreadyAllocated
                | BlockStateError::AlreadyPending,
            ) => return Err(RunError::DoubleFree),
            Err(BlockStateError::InvalidIndex) => return Err(RunError::InvalidPointer),
        }

        let Some(live) = state.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        state.live = live;
        state.push_free(ptr);

        Ok(RunFreeStatus { was_full })
    }

    pub(crate) fn allocated_block_at(&self, ptr: NonNull<u8>) -> Result<RunBlock, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        match self.blocks.is_allocated(block.index()) {
            Ok(true) => Ok(block),
            Ok(false) => Err(RunError::DoubleFree),
            Err(
                BlockStateError::InvalidIndex
                | BlockStateError::AlreadyFree
                | BlockStateError::AlreadyAllocated
                | BlockStateError::AlreadyPending,
            ) => Err(RunError::InvalidPointer),
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
        let offset = self.range().offset_of(ptr)?;
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

        RunBlock::at_offset(index, self.range().base(), self.block_size).map(RunBlock::ptr)
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
    fn new(block_size: usize) -> Self {
        debug_assert!(block_size >= size_of::<usize>());

        Self {
            live: 0,
            bump: 0,
            available_next: None,
            free: None,
        }
    }

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
    use core::alloc::Layout;

    use crate::{layout::LayoutSpec, memory::OsMemory, size_class::SizeClasses};

    use super::*;

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    fn class_id(size: usize, align: usize) -> SizeClassId {
        SizeClasses::id_for(layout_spec(size, align)).unwrap()
    }

    fn test_heap_id() -> HeapId {
        HeapId::new(0, NonZeroU32::MIN).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(0).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let capacity = RUN_SIZE / SizeClasses::block_size(class);
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = run.allocate().unwrap();
            let block = run.block_at(ptr).unwrap();
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
        let class = class_id(128, 8);
        let run = Run::new(
            RunId::from_index(1).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");

        let ptr = run.allocate().unwrap();

        assert!(run.free_local(ptr).is_ok());

        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            test_heap_id(),
            mapping,
            class_id(64, 8),
        )
        .expect("test run");
        let new = layout_spec(64, 8);
        let ptr = run.allocate().unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            test_heap_id(),
            mapping,
            class_id(64, 8),
        )
        .expect("test run");
        let new = layout_spec(80, 8);
        let ptr = run.allocate().unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_rejects_interior_pointer_for_non_power_of_two_class() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(24, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(ptr).is_some());
        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_reports_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert!(run.free_local(ptr).is_ok());
        assert!(matches!(run.free_local(ptr), Err(RunError::DoubleFree)));
    }

    #[test]
    fn remote_pending_run_reports_duplicate_remote_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(9).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim_free(ptr), Ok(()));
        assert_eq!(run.claim_free(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn remote_pending_run_unclaim_restores_allocated() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(12).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim_free(ptr), Ok(()));
        assert_eq!(run.unclaim(ptr), Ok(()));
        assert!(run.free_local(ptr).is_ok());
    }

    #[test]
    fn remote_pending_run_reports_local_free_as_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(10).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim_free(ptr), Ok(()));
        assert!(matches!(run.free_local(ptr), Err(RunError::DoubleFree)));
    }

    #[test]
    fn remote_pending_run_completes_to_reusable() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(11).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim_free(ptr), Ok(()));
        assert!(run.complete_remote_free(ptr).is_ok());
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_rejects_never_allocated_block_as_double_free() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");

        assert!(matches!(
            run.free_local(run.range().base()),
            Err(RunError::DoubleFree)
        ));
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_id(17, 16);
        let run = Run::new(
            RunId::from_index(3).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");
        let capacity = RUN_SIZE / SizeClasses::block_size(class);

        for _ in 0..capacity {
            let ptr = run.allocate().unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn run_range_reports_mapping_range() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let range = mapping.range();
        let run = Run::new(
            RunId::from_index(5).unwrap(),
            test_heap_id(),
            mapping,
            class_id(8, 8),
        )
        .expect("test run");

        assert_eq!(run.range().base(), range.base());
        assert_eq!(run.range().len(), range.len());
    }
}
