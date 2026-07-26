use core::{cell::UnsafeCell, mem::size_of, num::NonZeroU32, ptr::NonNull};

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    InvalidPointer,
    DoubleFree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockStateError {
    Conflict,
}

/// Per-block clear / Free / `RemotePending`.
///
/// Free/Live **authority** is freelist membership (+ bump). These bits keep
/// double-free and remote CAS fail-closed without restoring owner Allocated
/// stores on the bump allocate path (zero-filled ⇒ clear = live-or-never).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockState {
    Clear = 0,
    RemotePending = 1,
    Free = 2,
}

impl BlockState {
    const fn raw(self) -> u8 {
        match self {
            Self::Clear => 0,
            Self::RemotePending => 1,
            Self::Free => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            value if value == Self::Clear.raw() => Some(Self::Clear),
            value if value == Self::RemotePending.raw() => Some(Self::RemotePending),
            value if value == Self::Free.raw() => Some(Self::Free),
            _ => None,
        }
    }
}

struct BlockStates {
    bytes: AddressRange,
}

impl BlockStates {
    /// `index` must be capacity-proven.
    fn state(&self, index: BlockIndex) -> BlockState {
        let raw = self.state_unchecked(index).load(Ordering::Relaxed);
        debug_assert!(BlockState::from_raw(raw).is_some());
        // Only `Clear` / `Free` / `RemotePending` are ever stored; corrupt → Free (fail closed).
        BlockState::from_raw(raw).unwrap_or(BlockState::Free)
    }

    /// CAS `from` → `to` for a capacity-proven index.
    fn update(
        &self,
        index: BlockIndex,
        from: BlockState,
        to: BlockState,
    ) -> Result<(), BlockStateError> {
        match self.state_unchecked(index).compare_exchange(
            from.raw(),
            to.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(BlockStateError::Conflict),
        }
    }

    /// Atom for a capacity-proven block index (one address calc per op).
    fn state_unchecked(&self, index: BlockIndex) -> &AtomicU8 {
        let ptr = index.byte_unchecked(self.bytes);

        // SAFETY: `byte_unchecked` selected a byte in the run mapping's state
        // tail, zero-filled as clear, owned by `Run` for this value's lifetime,
        // and shared only through owner-local / atomic remote protocols.
        unsafe { &*ptr.as_ptr().cast::<AtomicU8>() }
    }
}

pub(crate) struct Run {
    /// Owner-local freelist / live / bump — field order prefers sticky locality under
    /// `repr(Rust)` (not a layout guarantee; do not treat as ABI).
    state: UnsafeCell<RunState>,
    /// Cached `mapping.base()` — payload span start (`RUN_SIZE` bytes).
    payload_base: NonNull<u8>,
    blocks: BlockStates,
    /// `trailing_zeros(block_size)` when power-of-two; `None` ⇒ multiply path.
    block_shift: Option<NonZeroU32>,
    class: SizeClassId,
    capacity: usize,
    block_size: usize,
    id: RunId,
    heap: HeapId,
    mapping: Mapping,
    /// Mirror of `RunState.bump` for remote `claim`. Cold.
    issued: AtomicUsize,
}

// SAFETY: owner-local methods are called only by the owning heap. Remote methods only touch
// `BlockStates`, load `issued`, and never mutate `RunState`.
unsafe impl Sync for Run {}

/// Empty freelist head / end-of-list link. Index `0` is a valid block, so this
/// is a deliberate sentinel (same pattern as `Arena`'s freelist).
const FREE_END: usize = usize::MAX;

struct RunState {
    live: usize,
    bump: usize,
    available_next: Option<NonNull<Run>>,
    /// `FREE_END` or a capacity-proven block index (raw, untagged).
    free: usize,
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

    const fn block_size_shift(block_size: usize) -> Option<NonZeroU32> {
        if block_size.is_power_of_two() {
            // Min size class is 8 (`trailing_zeros` ≥ 3); never zero for our table.
            NonZeroU32::new(block_size.trailing_zeros())
        } else {
            None
        }
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
        // `[RUN_SIZE, RUN_SIZE + capacity)` is exclusively Free/pending bits,
        // zero-filled as clear, and outlives `blocks` because `Self` owns
        // `mapping`.
        let state_base = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(RUN_SIZE)) };
        let blocks = BlockStates {
            bytes: AddressRange::new(state_base, capacity),
        };
        Some(Self {
            state: UnsafeCell::new(RunState::new(block_size)),
            payload_base: mapping.base(),
            blocks,
            block_shift: Self::block_size_shift(block_size),
            class,
            capacity,
            block_size,
            id,
            heap,
            mapping,
            issued: AtomicUsize::new(0),
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

    /// True when every block is outstanding (allocated or remote-pending).
    ///
    /// Used by `RunHeap` before `free` / `accept` for available-list relink.
    pub(crate) fn is_full(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &*self.state.get() }.live == self.capacity
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
        AddressRange::new(self.payload_base, RUN_SIZE)
    }

    #[inline]
    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let index = match self.pop_free(state) {
            Some(index) => {
                // Freelist entries are owner-published Free bits.
                let cleared = self
                    .blocks
                    .update(index, BlockState::Free, BlockState::Clear)
                    .is_ok();
                debug_assert!(cleared);
                index
            }
            None => self.allocate_fresh(state)?,
        };
        let ptr = self.block_ptr(index);

        debug_assert!(state.live < self.capacity);
        state.live += 1;
        Some(ptr)
    }

    /// Owner-local: live → freelist. Freelist (+ bump) is Free/Live authority;
    /// Free bit keeps delayed double-free fail-closed without bump-path stores.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.owner_block(ptr)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };

        self.blocks
            .update(block.index(), BlockState::Clear, BlockState::Free)?;

        debug_assert!(state.live > 0);
        state.live -= 1;
        Self::push_free(state, block);
        Ok(())
    }

    /// Owner path: containment + index without `Option` `block_at`.
    ///
    /// Includes bump / Free / `RemotePending` poison. Does not mutate freelist/`live`.
    #[inline]
    pub(crate) fn owner_block(&self, ptr: NonNull<u8>) -> Result<RunBlock, RunError> {
        // Out-of-span (incl. below base) wraps to a large offset ≥ `RUN_SIZE`.
        let offset = ptr
            .as_ptr()
            .addr()
            .wrapping_sub(self.payload_base.as_ptr().addr());
        if offset >= RUN_SIZE {
            return Err(RunError::InvalidPointer);
        }

        let Some(index) = self.block_index(offset) else {
            return Err(RunError::InvalidPointer);
        };
        if index >= self.capacity {
            return Err(RunError::InvalidPointer);
        }

        let index = BlockIndex::new(index);
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &*self.state.get() };
        if index.get() >= state.bump {
            return Err(RunError::DoubleFree);
        }
        match self.blocks.state(index) {
            BlockState::Clear => Ok(RunBlock::new(index, ptr)),
            BlockState::Free | BlockState::RemotePending => Err(RunError::DoubleFree),
        }
    }

    /// Freer: live → `RemotePending` (before batch/publish).
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        if block.index().get() >= self.issued.load(Ordering::Relaxed) {
            return Err(RunError::DoubleFree);
        }

        self.blocks
            .update(block.index(), BlockState::Clear, BlockState::RemotePending)?;
        Ok(())
    }

    pub(crate) fn unclaim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        self.blocks
            .update(block.index(), BlockState::RemotePending, BlockState::Clear)?;
        Ok(())
    }

    /// Owner: `RemotePending` → freelist (inbox flush / publish Draining complete).
    pub(crate) fn accept(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };

        self.blocks
            .update(block.index(), BlockState::RemotePending, BlockState::Free)?;

        debug_assert!(state.live > 0);
        state.live -= 1;
        Self::push_free(state, block);
        Ok(())
    }

    pub(crate) fn allocated_block_at(&self, ptr: NonNull<u8>) -> Result<RunBlock, RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &*self.state.get() };
        if block.index().get() >= state.bump {
            return Err(RunError::DoubleFree);
        }
        match self.blocks.state(block.index()) {
            BlockState::Clear => Ok(block),
            BlockState::Free | BlockState::RemotePending => Err(RunError::DoubleFree),
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

    #[inline]
    pub(crate) fn block_at(&self, ptr: NonNull<u8>) -> Option<RunBlock> {
        // Out-of-span (incl. below base) wraps to a large offset ≥ `RUN_SIZE`.
        let offset = ptr
            .as_ptr()
            .addr()
            .wrapping_sub(self.payload_base.as_ptr().addr());
        if offset >= RUN_SIZE {
            return None;
        }

        let index = self.block_index(offset)?;
        if index >= self.capacity {
            return None;
        }

        Some(RunBlock::new(BlockIndex::new(index), ptr))
    }

    /// Payload pointer for a freelist or bump index in `0..capacity`.
    #[inline]
    fn block_ptr(&self, index: BlockIndex) -> NonNull<u8> {
        debug_assert!(index.get() < self.capacity);
        let byte_offset = match self.block_shift {
            Some(shift) => index.get() << shift.get(),
            None => index.get() * self.block_size,
        };
        // SAFETY: freelist / `allocate_fresh` only yield `index < capacity`, so
        // `byte_offset < RUN_SIZE` inside the payload span.
        unsafe { NonNull::new_unchecked(self.payload_base.as_ptr().add(byte_offset)) }
    }

    #[cold]
    #[inline(never)]
    fn allocate_fresh(&self, state: &mut RunState) -> Option<BlockIndex> {
        if state.bump >= self.capacity {
            return None;
        }
        let index = BlockIndex::new(state.bump);
        state.bump += 1;
        self.issued.store(state.bump, Ordering::Relaxed);
        Some(index)
    }

    #[inline]
    fn pop_free(&self, state: &mut RunState) -> Option<BlockIndex> {
        let raw = state.free;
        if raw == FREE_END {
            return None;
        }

        let index = BlockIndex::new(raw);
        let ptr = self.block_ptr(index);
        state.free = Self::read_link(ptr);
        Some(index)
    }

    /// Push using the payload pointer already proven by `block_at` / `RunBlock`.
    #[inline]
    fn push_free(state: &mut RunState, block: RunBlock) {
        Self::write_link(block.ptr(), state.free);
        state.free = block.index().get();
    }

    #[inline]
    fn read_link(ptr: NonNull<u8>) -> usize {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        unsafe { ptr.cast::<usize>().as_ptr().read() }
    }

    #[inline]
    fn write_link(ptr: NonNull<u8>, word: usize) {
        // SAFETY: free-list links are stored only in reusable blocks owned by this run.
        unsafe {
            ptr.cast::<usize>().as_ptr().write(word);
        }
    }

    #[inline]
    fn block_index(&self, offset: usize) -> Option<usize> {
        if let Some(shift) = self.block_shift {
            if offset & (self.block_size - 1) != 0 {
                return None;
            }

            return Some(offset >> shift.get());
        }

        if !offset.is_multiple_of(self.block_size) {
            return None;
        }

        offset.checked_div(self.block_size)
    }
}

impl From<BlockStateError> for RunError {
    fn from(_error: BlockStateError) -> Self {
        Self::DoubleFree
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
    fn reusable_run_reports_delayed_double_free() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(7).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let a = run.allocate().unwrap();
        let b = run.allocate().unwrap();

        assert!(run.free(a).is_ok());
        assert!(run.free(b).is_ok());
        assert!(matches!(run.free(a), Err(RunError::DoubleFree)));
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
    fn run_owner_block_rejects_interior_pointer() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(0).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");

        let ptr = run.allocate().unwrap();
        assert_eq!(run.owner_block(ptr).unwrap().ptr(), ptr);
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };
        assert!(run.owner_block(interior).is_err());
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
