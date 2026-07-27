use core::{
    cell::UnsafeCell,
    mem::size_of,
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};

pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
    size_class::SizeClass,
};

use super::{
    HeapId,
    table::inbox::{Notified, Notify},
};

pub(crate) use heap::{RunHeap, RunHeapError};

pub(crate) const RUN_SIZE: usize = 64 * 1024;
/// Bits per claim-bitmap word (`AtomicU64`).
const CLAIM_WORD_BITS: usize = 64;

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
        // SAFETY: caller proved `get() < bytes.len()` via `locate` / freelist / bump.
        unsafe { NonNull::new_unchecked(bytes.base().as_ptr().add(self.get())) }
    }

    fn claim_word_bit(self) -> (usize, u64) {
        let index = self.get();
        let word = index / CLAIM_WORD_BITS;
        let bit = index % CLAIM_WORD_BITS;
        (word, 1_u64 << bit)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Block {
    index: BlockIndex,
    ptr: NonNull<u8>,
}

impl Block {
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

/// Per-block clear / Free.
///
/// Free/Live **authority** is freelist membership (+ bump). The Free bit keeps
/// delayed double-free fail-closed. Remote admission is owned exclusively by
/// [`ClaimBits`] — there is no third byte state.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockState {
    Clear = 0,
    Free = 2,
}

impl BlockState {
    const fn raw(self) -> u8 {
        match self {
            Self::Clear => 0,
            Self::Free => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            value if value == Self::Clear.raw() => Some(Self::Clear),
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
    fn state(&self, index: BlockIndex, order: Ordering) -> BlockState {
        let raw = self.state_unchecked(index).load(order);
        debug_assert!(BlockState::from_raw(raw).is_some());
        // Only `Clear` / `Free` are ever stored; corrupt → Free (fail closed).
        BlockState::from_raw(raw).unwrap_or(BlockState::Free)
    }

    /// Unconditional write (owner freelist allocate / free handshake / accept).
    #[inline]
    fn set(&self, index: BlockIndex, to: BlockState, order: Ordering) {
        self.state_unchecked(index).store(to.raw(), order);
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

/// Run-owned remote-admission bitmap.
///
/// Exactly one of owner `free` or remote `claim` linearizes a live block:
/// - `claim`: `try_set` then Acquire-load Free; undo bit if Free already.
/// - `free`: store Free (Release) then Acquire-load bit; undo Free if claimed.
/// - `accept`: `test_and_clear` then publish to freelist.
struct ClaimBits {
    /// 8-aligned claim words in the run mapping tail.
    words: NonNull<AtomicU64>,
    word_count: usize,
}

impl ClaimBits {
    fn byte_len(capacity: usize) -> Option<usize> {
        let words = capacity.div_ceil(CLAIM_WORD_BITS);
        words.checked_mul(size_of::<u64>())
    }

    /// Byte offset of the claim span from the mapping base (`RUN_SIZE + capacity`, 8-aligned).
    fn mapping_offset(capacity: usize) -> Option<usize> {
        RUN_SIZE
            .checked_add(capacity)?
            .checked_next_multiple_of(size_of::<u64>())
    }

    fn word_count(capacity: usize) -> usize {
        capacity.div_ceil(CLAIM_WORD_BITS)
    }

    #[inline]
    fn try_set(&self, index: BlockIndex) -> bool {
        let (word, mask) = index.claim_word_bit();
        let prev = self.word_unchecked(word).fetch_or(mask, Ordering::AcqRel);
        prev & mask == 0
    }

    #[inline]
    fn is_set(&self, index: BlockIndex) -> bool {
        let (word, mask) = index.claim_word_bit();
        self.word_unchecked(word).load(Ordering::Acquire) & mask != 0
    }

    #[inline]
    fn clear(&self, index: BlockIndex) {
        let (word, mask) = index.claim_word_bit();
        self.word_unchecked(word)
            .fetch_and(!mask, Ordering::Release);
    }

    /// Atomically take every bit in `word`, returning the bits that were set beforehand.
    #[inline]
    fn drain_word(&self, word: usize) -> u64 {
        self.word_unchecked(word).swap(0, Ordering::AcqRel)
    }

    /// Cheap post-scan check for a straggling claim a bulk drain may have missed.
    #[inline]
    fn any_set(&self) -> bool {
        (0..self.word_count).any(|word| self.word_unchecked(word).load(Ordering::Acquire) != 0)
    }

    fn word_unchecked(&self, word: usize) -> &AtomicU64 {
        debug_assert!(word < self.word_count);
        // SAFETY: `word < word_count`; `words` points at the claim span carved
        // from this run's mapping and aligned for `AtomicU64`.
        unsafe { &*self.words.as_ptr().add(word) }
    }
}

pub(crate) struct Run {
    /// Owner-local freelist / live / bump — field order prefers sticky locality under
    /// `repr(Rust)` (not a layout guarantee; do not treat as ABI).
    state: UnsafeCell<RunState>,
    /// Cached `mapping.base()` — payload span start (`RUN_SIZE` bytes).
    base: NonNull<u8>,
    blocks: BlockStates,
    claims: ClaimBits,
    /// `trailing_zeros(stride)` when power-of-two; `None` means multiply.
    stride_shift: Option<NonZeroU32>,
    class: SizeClass,
    capacity: usize,
    stride: usize,
    id: RunId,
    heap: HeapId,
    mapping: Mapping,
    /// Mirror of `RunState.bump` for remote `claim`. Cold.
    issued: AtomicUsize,
    /// Coalesced-by-run remote-notify link (see `heap::table::inbox`). Cold.
    notify: Notify<Run>,
}

// SAFETY: owner-local methods are called only by the owning heap. Remote methods only touch
// `BlockStates` / `ClaimBits` / `Notify`, load `issued`, and never mutate `RunState` (except
// `accept_remote`, itself an owner-local method called only through the owning heap's flush).
unsafe impl Sync for Run {}

impl Notified for Run {
    fn notify(&self) -> &Notify<Self> {
        &self.notify
    }
}

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
    /// Bytes for one run mapping: payload + Free bytes + pad + claim bitmap words.
    pub(crate) fn mapping_len(class: SizeClass) -> Option<usize> {
        let stride = class.size();
        let capacity = RUN_SIZE.checked_div(stride).filter(|&count| count > 0)?;
        let claim_bytes = ClaimBits::byte_len(capacity)?;
        let claim_offset = ClaimBits::mapping_offset(capacity)?;
        claim_offset.checked_add(claim_bytes)
    }

    pub(crate) fn new(id: RunId, heap: HeapId, mapping: Mapping, class: SizeClass) -> Option<Self> {
        let stride = class.size();
        let capacity = RUN_SIZE.checked_div(stride).filter(|&count| count > 0)?;
        let claim_bytes = ClaimBits::byte_len(capacity)?;
        let claim_offset = ClaimBits::mapping_offset(capacity)?;
        let need = claim_offset.checked_add(claim_bytes)?;
        if mapping.len().get() < need {
            return None;
        }

        // SAFETY: `mapping` covers at least `need` bytes. The Free-byte tail
        // starts at `RUN_SIZE` and the claim-word span at `claim_offset`
        // (8-aligned). Both are zero-filled and outlive `blocks` / `claims`
        // because `Self` owns `mapping`.
        let state_base = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(RUN_SIZE)) };
        #[allow(clippy::cast_ptr_alignment)] // `claim_offset` is 8-aligned above.
        // SAFETY: `claim_offset` is 8-aligned (`mapping_offset`) and within
        // `need`; the span holds `word_count` zeroed `AtomicU64` slots for this
        // run's lifetime.
        let claim_words = unsafe {
            NonNull::new_unchecked(
                mapping
                    .base()
                    .as_ptr()
                    .add(claim_offset)
                    .cast::<AtomicU64>(),
            )
        };
        let blocks = BlockStates {
            bytes: AddressRange::new(state_base, capacity),
        };
        let claims = ClaimBits {
            words: claim_words,
            word_count: ClaimBits::word_count(capacity),
        };
        // Min size class is 8 (`trailing_zeros` ≥ 3); never zero for our table.
        let stride_shift = stride
            .is_power_of_two()
            .then(|| NonZeroU32::new(stride.trailing_zeros()))
            .flatten();
        Some(Self {
            state: UnsafeCell::new(RunState::new(stride)),
            base: mapping.base(),
            blocks,
            claims,
            stride_shift,
            class,
            capacity,
            stride,
            id,
            heap,
            mapping,
            issued: AtomicUsize::new(0),
            notify: Notify::new(),
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

    pub(crate) const fn class(&self) -> SizeClass {
        self.class
    }

    /// True when every block is outstanding (allocated or remote-claimed).
    ///
    /// Used by `RunHeap` before `free` / `accept` for available-list relink.
    pub(crate) fn is_full(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &*self.state.get() }.live == self.capacity
    }

    /// Outstanding blocks on this run (allocated or remote-claimed).
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
        AddressRange::new(self.base, RUN_SIZE)
    }

    #[inline]
    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let index = match self.pop_free(state) {
            Some(index) => {
                // Freelist membership is Free/Live authority on the owner path.
                // Reject an in-flight remote claim (should be empty ∩ freelist).
                if self.claims.is_set(index) {
                    return None;
                }
                if self.blocks.state(index, Ordering::Relaxed) != BlockState::Free {
                    return None;
                }
                self.blocks.set(index, BlockState::Clear, Ordering::Relaxed);
                index
            }
            None => self.allocate_fresh(state)?,
        };
        let ptr = self.address(index);

        debug_assert!(state.live < self.capacity);
        state.live += 1;
        Some(ptr)
    }

    /// Owner-local: live → freelist without locked RMW.
    ///
    /// Handshake vs remote `claim`: store Free (Release), then recheck claim bit
    /// (Acquire). If the bit is set, undo Free→Clear and fail closed — `accept`
    /// owns the freelist publish.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.locate(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        if block.index().get() >= state.bump {
            return Err(RunError::DoubleFree);
        }

        if self.blocks.state(block.index(), Ordering::Relaxed) != BlockState::Clear {
            return Err(RunError::DoubleFree);
        }
        self.blocks
            .set(block.index(), BlockState::Free, Ordering::Release);
        if self.claims.is_set(block.index()) {
            self.blocks
                .set(block.index(), BlockState::Clear, Ordering::Relaxed);
            return Err(RunError::DoubleFree);
        }

        debug_assert!(state.live > 0);
        state.live -= 1;
        Self::push_free(state, block);
        Ok(())
    }

    /// Freer: reserve remote admission before batch/publish payload reuse.
    ///
    /// Handshake vs owner `free`: set claim bit, then Acquire-load Free. If Free
    /// already, clear the bit and fail closed.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.locate(ptr).ok_or(RunError::InvalidPointer)?;
        if block.index().get() >= self.issued.load(Ordering::Relaxed) {
            return Err(RunError::DoubleFree);
        }

        if !self.claims.try_set(block.index()) {
            return Err(RunError::DoubleFree);
        }
        if self.blocks.state(block.index(), Ordering::Acquire) == BlockState::Free {
            self.claims.clear(block.index());
            return Err(RunError::DoubleFree);
        }
        Ok(())
    }

    /// Idle → Queued for this run's remote-notify slot.
    ///
    /// `true` means the caller must publish this run on the owning heap's run inbox — see
    /// [`Self::accept_remote`] for the paired disarm and wakeup proof.
    #[inline]
    pub(crate) fn try_arm(&self) -> bool {
        self.notify.try_arm()
    }

    /// Owner: drain every claimed bit on this run and publish the freed blocks.
    ///
    /// Wakeup proof (idle-first + recheck): disarms this run's notify *before* scanning, so
    /// a racing `claim` + `try_arm` is free to re-queue the run the moment it is dequeued
    /// from the inbox (its `next` link is only safe to reuse once Idle). A straggling claim
    /// that lands in an already-scanned word is never dropped: either that racer's own
    /// `try_arm` wins (Idle at the time) and republishes on its own, or it loses (Queued
    /// already, e.g. from a concurrent flush) and this call's own re-arm below wins instead —
    /// exactly one of the two republishes, so the run is always requeued when work remains.
    ///
    /// Returns `Result` (never currently `Err`) for parity with the other domain ops
    /// (`claim` / `free`) that `RunHeap::accept_remote` propagates via `?`.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn accept_remote(&self) -> Result<bool, RunError> {
        self.notify.disarm();

        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        for word in 0..self.claims.word_count {
            let mut bits = self.claims.drain_word(word);
            while bits != 0 {
                // `trailing_zeros` of a nonzero `u64` is always < 64, so this never truncates.
                let bit = usize::try_from(bits.trailing_zeros()).unwrap();
                bits &= bits - 1;
                let index = BlockIndex::new(word * CLAIM_WORD_BITS + bit);
                debug_assert!(index.get() < self.capacity);
                self.blocks.set(index, BlockState::Free, Ordering::Relaxed);
                debug_assert!(state.live > 0);
                state.live -= 1;
                Self::push_free(state, Block::new(index, self.address(index)));
            }
        }

        Ok(self.claims.any_set() && self.notify.try_arm())
    }

    pub(crate) fn allocated(&self, ptr: NonNull<u8>) -> Result<Block, RunError> {
        let block = self.locate(ptr).ok_or(RunError::InvalidPointer)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &*self.state.get() };
        if block.index().get() >= state.bump {
            return Err(RunError::DoubleFree);
        }
        if self.claims.is_set(block.index()) {
            return Err(RunError::DoubleFree);
        }
        match self.blocks.state(block.index(), Ordering::Relaxed) {
            BlockState::Clear => Ok(block),
            BlockState::Free => Err(RunError::DoubleFree),
        }
    }

    pub(crate) fn resize_in_place(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, RunError> {
        self.allocated(ptr)?;

        Ok(self.stride >= spec.size() && spec.is_addr_aligned(ptr.as_ptr().addr()))
    }

    #[inline]
    pub(crate) fn locate(&self, ptr: NonNull<u8>) -> Option<Block> {
        // Out-of-span (incl. below base) wraps to a large offset ≥ `RUN_SIZE`.
        let offset = ptr.as_ptr().addr().wrapping_sub(self.base.as_ptr().addr());
        if offset >= RUN_SIZE {
            return None;
        }

        let index = self.class.index_of(offset)?;
        if index >= self.capacity {
            return None;
        }

        Some(Block::new(BlockIndex::new(index), ptr))
    }

    /// Payload pointer for a freelist or bump index in `0..capacity`.
    #[inline]
    fn address(&self, index: BlockIndex) -> NonNull<u8> {
        debug_assert!(index.get() < self.capacity);
        let byte_offset = match self.stride_shift {
            Some(shift) => index.get() << shift.get(),
            None => index.get() * self.stride,
        };
        // SAFETY: freelist / `allocate_fresh` only yield `index < capacity`, so
        // `byte_offset < RUN_SIZE` inside the payload span.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().add(byte_offset)) }
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
        let ptr = self.address(index);
        state.free = Self::read_link(ptr);
        Some(index)
    }

    /// Push using the payload pointer already proven by `locate` / `Block`.
    #[inline]
    fn push_free(state: &mut RunState, block: Block) {
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

    fn class_id(size: usize, align: usize) -> SizeClass {
        SizeClasses::class_for(layout_spec(size, align)).unwrap()
    }

    fn test_heap_id() -> HeapId {
        HeapId::new(0, NonZeroU32::MIN).unwrap()
    }

    fn map_for_class(class: SizeClass) -> Mapping {
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
        let capacity = RUN_SIZE / class.size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = run.allocate().unwrap();
            let block = run.locate(ptr).unwrap();
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
    fn freelist_allocate_rejects_non_free_state() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(2).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");

        let ptr = run.allocate().unwrap();
        assert!(run.free(ptr).is_ok());
        let index = run.locate(ptr).unwrap().index();
        // Corrupt DF bit while the block remains on the freelist.
        run.blocks.set(index, BlockState::Clear, Ordering::Relaxed);
        assert!(run.allocate().is_none());
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

        assert!(run.locate(interior).is_none());
    }

    #[test]
    fn reusable_run_locate_covers_all_classes_boundaries_and_tail_slack() {
        for (run_index, &size) in SizeClasses::SIZES.iter().enumerate() {
            let class = class_id(size, 8);
            let run = Run::new(
                RunId::from_index(u32::try_from(run_index).unwrap()).unwrap(),
                test_heap_id(),
                map_for_class(class),
                class,
            )
            .expect("test run");
            let capacity = RUN_SIZE / size;

            let first = run.allocate().unwrap();
            assert!(run.locate(first).is_some(), "size={size}");
            assert!(
                run.locate(unsafe { NonNull::new_unchecked(first.as_ptr().add(1)) })
                    .is_none(),
                "size={size}"
            );

            let slack_offset = capacity * size;
            if slack_offset < RUN_SIZE {
                let slack = unsafe {
                    NonNull::new_unchecked(run.range().base().as_ptr().add(slack_offset))
                };
                assert!(run.locate(slack).is_none(), "size={size} slack");
            }
        }
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

        assert!(run.locate(ptr).is_some());
        assert!(run.locate(interior).is_none());
    }

    #[test]
    fn reusable_run_round_trips_hotspot_non_power_of_two_classes() {
        for (run_index, size) in [80, 96].into_iter().enumerate() {
            let class = class_id(size, 8);
            let run = Run::new(
                RunId::from_index(u32::try_from(run_index).unwrap()).unwrap(),
                test_heap_id(),
                map_for_class(class),
                class,
            )
            .expect("test run");
            let ptr = run.allocate().unwrap();

            assert!(run.locate(ptr).is_some(), "size={size}");
            assert!(run.free(ptr).is_ok(), "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_rejects_aligned_tail_slack() {
        for (run_index, size) in [80, 96].into_iter().enumerate() {
            let class = class_id(size, 8);
            let run = Run::new(
                RunId::from_index(u32::try_from(run_index).unwrap()).unwrap(),
                test_heap_id(),
                map_for_class(class),
                class,
            )
            .expect("test run");
            let capacity = RUN_SIZE / class.size();
            let slack_offset = capacity * class.size();
            assert!(slack_offset < RUN_SIZE, "size={size}");
            let slack =
                unsafe { NonNull::new_unchecked(run.range().base().as_ptr().add(slack_offset)) };

            assert!(run.locate(slack).is_none(), "size={size}");
        }
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
    fn claim_run_reports_duplicate_remote_free() {
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
    fn claim_then_owner_free_reports_double_free_and_accept_completes() {
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
        assert_eq!(run.accept_remote(), Ok(false));
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn claim_run_completes_to_reusable() {
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
        assert_eq!(run.accept_remote(), Ok(false));
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn free_then_claim_reports_double_free_and_bit_clears() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(12).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let index = run.locate(ptr).unwrap().index();

        assert!(run.free(ptr).is_ok());
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
        assert!(!run.claims.is_set(index));
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn free_after_claim_bit_set_before_free_store_loses_to_claim() {
        // Deterministic interleaving: claim bit is set before owner Free store.
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(13).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let index = run.locate(ptr).unwrap().index();

        assert!(run.claims.try_set(index));
        assert!(matches!(run.free(ptr), Err(RunError::DoubleFree)));
        assert!(run.claims.is_set(index));
        assert_eq!(
            run.blocks.state(index, Ordering::Relaxed),
            BlockState::Clear
        );
        assert_eq!(run.accept_remote(), Ok(false));
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn claim_after_free_store_before_claim_bit_loses_to_free() {
        // Deterministic interleaving: Free is stored before claim try_set.
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(14).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let index = run.locate(ptr).unwrap().index();

        run.blocks.set(index, BlockState::Free, Ordering::Release);
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
        assert!(!run.claims.is_set(index));
        // Owner already linearized Free; finish the freelist publish that a
        // full `free` would have done after a successful handshake.
        // SAFETY: owner-local test harness.
        let state = unsafe { &mut *run.state.get() };
        state.live -= 1;
        Run::push_free(state, run.locate(ptr).unwrap());
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn allocate_rejects_freelist_candidate_with_in_flight_claim() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(15).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        let index = run.locate(ptr).unwrap().index();
        assert!(run.free(ptr).is_ok());
        assert!(run.claims.try_set(index));
        assert!(run.allocate().is_none());
    }

    #[test]
    fn accept_remote_without_any_claim_is_a_noop() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(16).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let ptr = run.allocate().unwrap();
        assert_eq!(run.accept_remote(), Ok(false));
        // `ptr`'s block is still live (never claimed), so the next allocate is fresh.
        assert_ne!(run.allocate().unwrap(), ptr);
    }

    #[test]
    fn claim_accept_remote_works_for_all_size_classes() {
        for (run_index, &size) in SizeClasses::SIZES.iter().enumerate() {
            let class = class_id(size, 8);
            let run = Run::new(
                RunId::from_index(u32::try_from(run_index).unwrap()).unwrap(),
                test_heap_id(),
                map_for_class(class),
                class,
            )
            .expect("test run");
            let ptr = run.allocate().unwrap();
            assert_eq!(run.claim(ptr), Ok(()), "size={size}");
            assert_eq!(run.accept_remote(), Ok(false), "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
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
        let capacity = RUN_SIZE / class.size();

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

    #[test]
    fn try_arm_wins_once_until_disarmed() {
        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(17).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let a = run.allocate().unwrap();
        let b = run.allocate().unwrap();

        assert_eq!(run.claim(a), Ok(()));
        // First claim on an idle run wins the arm race and must publish.
        assert!(run.try_arm());

        assert_eq!(run.claim(b), Ok(()));
        // A second claim while still queued must not publish again.
        assert!(!run.try_arm());

        // accept_remote coalesces both claims from the single queued entry.
        assert_eq!(run.accept_remote(), Ok(false));
        assert_eq!(run.allocate(), Some(b));
        assert_eq!(run.allocate(), Some(a));

        // Disarmed by accept_remote: a fresh claim can arm again.
        assert_eq!(run.claim(a), Ok(()));
        assert!(run.try_arm());
    }

    /// Faithful simulation of the real `HeapSlot::flush` loop: a freer claims and
    /// arms/notifies concurrently with an "owner" that drains the inbox and republishes
    /// on `Ok(true)`. No claim may ever be stranded (wakeup proof).
    #[test]
    fn accept_remote_wakeup_proof_no_claim_is_ever_stranded() {
        use core::sync::atomic::AtomicBool;

        use super::super::table::inbox::Inbox;

        let class = class_id(64, 8);
        let run = Run::new(
            RunId::from_index(20).unwrap(),
            test_heap_id(),
            map_for_class(class),
            class,
        )
        .expect("test run");
        let capacity = RUN_SIZE / class.size();
        // Addresses, not `NonNull<u8>`: a raw-pointer `Vec` is not `Sync`, and this slice
        // only ever crosses the thread boundary by shared reference below.
        let addrs: Vec<usize> = (0..capacity)
            .map(|_| run.allocate().unwrap().as_ptr() as usize)
            .collect();
        let inbox: Inbox<Run> = Inbox::new();
        let done = AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let run_ptr = NonNull::from(&run);
                for &addr in &addrs {
                    // SAFETY: addr is one of this run's own blocks, allocated above.
                    let ptr = NonNull::new(addr as *mut u8).unwrap();
                    run.claim(ptr).unwrap();
                    if run.try_arm() {
                        inbox.republish(run_ptr);
                    }
                }
                done.store(true, Ordering::Release);
            });

            let mut spins = 0u32;
            loop {
                let finished = done.load(Ordering::Acquire);
                while let Some(chain) = inbox.drain() {
                    for r in chain {
                        // SAFETY: `r` is `run_ptr`, live for the scope of this test.
                        if unsafe { r.as_ref() }.accept_remote().unwrap() {
                            inbox.republish(r);
                        }
                    }
                }
                if finished && inbox.is_empty() && !run.claims.any_set() {
                    break;
                }
                spins += 1;
                assert!(spins < 100_000_000, "owner loop never observed quiescence");
                core::hint::spin_loop();
            }
        });

        assert!(!run.has_live_blocks());
    }
}
