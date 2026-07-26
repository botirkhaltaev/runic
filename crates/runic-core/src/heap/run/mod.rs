use core::{cell::UnsafeCell, mem::size_of, num::NonZeroU32, ptr::NonNull};

use core::sync::atomic::{AtomicU8, Ordering};

pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
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

    const fn get(self) -> usize {
        self.index
    }

    fn offset(self, block_size: usize) -> Option<usize> {
        self.get().checked_mul(block_size)
    }

    /// State-tail byte for an index already proven in `0..bytes.len()` (capacity).
    fn byte_unchecked(self, bytes: AddressRange) -> NonNull<u8> {
        debug_assert!(self.get() < bytes.len());
        // SAFETY: caller proved `get() < bytes.len()` via `block_at` / freelist / bump.
        unsafe { NonNull::new_unchecked(bytes.base().as_ptr().add(self.get())) }
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
/// One `AtomicU8` per block for this run's capacity, stored in the run mapping
/// immediately after the `RUN_SIZE` payload span (zero-filled ⇒ Free). Not a
/// packed bitmap; this is the only free/allocated/remote-pending tracker on a
/// run. `bytes` is a non-owning view of that state tail.
struct BlockStates {
    bytes: AddressRange,
}

impl BlockStates {
    /// Owner-local Free → Allocated. `index` must be capacity-proven.
    fn allocate(&self, index: BlockIndex) {
        let state = self.state_unchecked(index);
        debug_assert_eq!(
            BlockState::from_raw(state.load(Ordering::Relaxed)),
            Some(BlockState::Free)
        );
        state.store(BlockState::Allocated.raw(), Ordering::Relaxed);
    }

    /// `index` must be capacity-proven (`block_at` / freelist / bump).
    fn is_allocated(&self, index: BlockIndex) -> Result<bool, BlockStateError> {
        let raw = self.state_unchecked(index).load(Ordering::Relaxed);
        Ok(
            BlockState::from_raw(raw).ok_or(BlockStateError::InvalidIndex)?
                == BlockState::Allocated,
        )
    }

    /// Owner-local Allocated → Free. `index` must be capacity-proven.
    fn release(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state_unchecked(index);
        match BlockState::from_raw(state.load(Ordering::Relaxed))
            .ok_or(BlockStateError::InvalidIndex)?
        {
            BlockState::Allocated => {
                state.store(BlockState::Free.raw(), Ordering::Relaxed);
                Ok(())
            }
            BlockState::Free => Err(BlockStateError::AlreadyFree),
            BlockState::RemotePending => Err(BlockStateError::AlreadyPending),
        }
    }

    fn mark_remote_pending(&self, index: BlockIndex) -> Result<(), BlockStateError> {
        let state = self.state_unchecked(index);
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
        let state = self.state_unchecked(index);
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
        let state = self.state_unchecked(index);
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

    fn state_error(raw: u8) -> Result<(), BlockStateError> {
        match BlockState::from_raw(raw) {
            Some(BlockState::Free) => Err(BlockStateError::AlreadyFree),
            Some(BlockState::Allocated) => Err(BlockStateError::AlreadyAllocated),
            Some(BlockState::RemotePending) => Err(BlockStateError::AlreadyPending),
            None => Err(BlockStateError::InvalidIndex),
        }
    }

    /// Atom for a capacity-proven block index (one address calc per op).
    fn state_unchecked(&self, index: BlockIndex) -> &AtomicU8 {
        let ptr = index.byte_unchecked(self.bytes);

        // SAFETY: `byte_unchecked` selected a byte in the run mapping's state
        // tail, zero-filled as Free, owned by `Run` for this value's lifetime,
        // and shared only through owner-local / atomic remote protocols.
        unsafe { &*ptr.as_ptr().cast::<AtomicU8>() }
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

/// Empty freelist head / end-of-list link. Index `0` is a valid block, so this
/// is a deliberate sentinel (same pattern as `Arena`'s freelist).
const FREE_END: usize = usize::MAX;

struct RunState {
    live: usize,
    bump: usize,
    available_next: Option<NonNull<Run>>,
    /// `FREE_END` or a capacity-proven block index.
    free: usize,
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
    /// Bytes for one run mapping: `RUN_SIZE` payload plus one `AtomicU8` per block.
    pub(crate) fn mapping_len(class: SizeClassId) -> Option<usize> {
        let block_size = SizeClasses::block_size(class);
        let capacity = RUN_SIZE
            .checked_div(block_size)
            .filter(|&count| count > 0)?;
        RUN_SIZE.checked_add(capacity)
    }

    pub(crate) fn new(
        id: RunId,
        heap: HeapId,
        mapping: Mapping,
        class: SizeClassId,
    ) -> Option<Self> {
        let block_size = SizeClasses::block_size(class);
        let capacity = RUN_SIZE
            .checked_div(block_size)
            .filter(|&count| count > 0)?;
        let need = RUN_SIZE.checked_add(capacity)?;
        if mapping.len().get() < need {
            return None;
        }

        // SAFETY: `mapping` covers at least `need` bytes. The state tail
        // `[RUN_SIZE, RUN_SIZE + capacity)` is exclusively block-state storage,
        // zero-filled as Free, and outlives `blocks` because `Self` owns
        // `mapping`.
        let state_base = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(RUN_SIZE)) };
        let blocks = BlockStates {
            bytes: AddressRange::new(state_base, capacity),
        };
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

    /// Outstanding blocks on this run (allocated or remote-pending).
    pub(crate) fn has_live_blocks(&self) -> bool {
        // SAFETY: read under owner-local access or table-locked reclaim.
        unsafe { &*self.state.get() }.live != 0
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
        AddressRange::new(self.mapping.base(), RUN_SIZE)
    }

    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let index = self
            .pop_free(state)
            .or_else(|| state.allocate_fresh(self.capacity))?;
        let ptr = self.block_ptr(index);
        self.blocks.allocate(index);

        debug_assert!(state.live < self.capacity);
        state.live += 1;
        Some(ptr)
    }

    /// Owner-local: Allocated → Free, push freelist.
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
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
        Self::push_free(state, block);

        Ok(RunFreeStatus { was_full })
    }

    /// Freer: Allocated → `RemotePending` (before batch/publish).
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
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

    /// Owner: `RemotePending` → Free (inbox flush / publish Draining complete).
    pub(crate) fn accept(&self, ptr: NonNull<u8>) -> Result<RunFreeStatus, RunError> {
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
        Self::push_free(state, block);

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

    /// Payload pointer for a freelist or bump index in `0..capacity`.
    fn block_ptr(&self, index: BlockIndex) -> NonNull<u8> {
        debug_assert!(index.get() < self.capacity);
        // SAFETY: freelist / `allocate_fresh` only yield `index < capacity`.
        unsafe {
            RunBlock::at_offset(index, self.range().base(), self.block_size)
                .unwrap_unchecked()
                .ptr()
        }
    }

    fn pop_free(&self, state: &mut RunState) -> Option<BlockIndex> {
        let raw = state.free;
        if raw == FREE_END {
            return None;
        }

        let index = BlockIndex::new(raw);
        let ptr = self.block_ptr(index);
        state.free = Self::read_free_next(ptr);
        Some(index)
    }

    /// Push using the payload pointer already proven by `block_at` / `RunBlock`.
    fn push_free(state: &mut RunState, block: RunBlock) {
        Self::write_free_next(block.ptr(), state.free);
        state.free = block.index().get();
    }

    fn read_free_next(ptr: NonNull<u8>) -> usize {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        unsafe { ptr.cast::<usize>().as_ptr().read() }
    }

    fn write_free_next(ptr: NonNull<u8>, next: usize) {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        unsafe {
            ptr.cast::<usize>().as_ptr().write(next);
        }
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
            free: FREE_END,
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

    fn map_for_class(class: SizeClassId) -> Mapping {
        OsMemory::map(Run::mapping_len(class).unwrap()).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(0).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let capacity = RUN_SIZE / SizeClasses::block_size(class);
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = run.allocate().unwrap();
            let block = run.block_at(ptr).unwrap();
            let index = block.index().get();

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
        let class = class_id(128, 8);
        let run = Run::new(
            RunId::from_index(1).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");

        let ptr = run.allocate().unwrap();

        assert!(run.free(ptr).is_ok());

        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let new = layout_spec(64, 8);
        let ptr = run.allocate().unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let new = layout_spec(80, 8);
        let ptr = run.allocate().unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_rejects_interior_pointer_for_non_power_of_two_class() {
        let class = class_id(24, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            test_heap_id(),
            map_for_class(class),
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
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert!(run.free(ptr).is_ok());
        assert!(matches!(run.free(ptr), Err(RunError::DoubleFree)));
    }

    #[test]
    fn remote_pending_run_reports_duplicate_remote_free() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(9).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn remote_pending_run_unclaim_restores_allocated() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(12).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.unclaim(ptr), Ok(()));
        assert!(run.free(ptr).is_ok());
    }

    #[test]
    fn remote_pending_run_reports_local_free_as_double_free() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(10).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert!(matches!(run.free(ptr), Err(RunError::DoubleFree)));
    }

    #[test]
    fn remote_pending_run_completes_to_reusable() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(11).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert!(run.accept(ptr).is_ok());
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_rejects_never_allocated_block_as_double_free() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(8).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");

        assert!(matches!(
            run.free(run.range().base()),
            Err(RunError::DoubleFree)
        ));
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let class = class_id(17, 16);
        let run = Run::new(
            RunId::from_index(3).unwrap(),
            test_heap_id(),
            map_for_class(class),
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
    fn run_range_reports_payload_span() {
        let class = class_id(8, 8);
        let mapping = map_for_class(class);
        let base = mapping.base();
        let run = Run::new(
            RunId::from_index(5).unwrap(),
            test_heap_id(),
            mapping,
            class,
        )
        .expect("test run");

        assert_eq!(run.range().base(), base);
        assert_eq!(run.range().len(), RUN_SIZE);
        assert!(run.mapping().len().get() >= Run::mapping_len(class).unwrap());
    }
}
