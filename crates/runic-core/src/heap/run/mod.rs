use core::{
    cell::UnsafeCell,
    mem::{align_of, size_of},
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

pub(crate) mod cache;
pub(crate) mod config;
pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, OsMemory, PAGE_SIZE},
    size_class::SizeClass,
};

use super::{
    HeapId,
    inbox::{InboxLink, InboxNode},
};

use config::RunPolicy;

pub(crate) use cache::RunCache;
pub(crate) use heap::RunHeap;

pub(crate) const RUN_SIZE: usize = 64 * 1024;
/// Payload plus claim tail, `RUN_SIZE`-aligned.
pub(crate) const RUN_SPACE: usize = RUN_SIZE * 2;
/// Runs per heap-owned payload map.
pub(crate) const MAP_RUNS: usize = 16;
pub(crate) const MAP_SIZE: usize = MAP_RUNS * RUN_SPACE;

const _: () = assert!(RUN_SPACE >= RUN_SIZE + (RUN_SIZE / 8).div_ceil(64) * 8);
const _: () = assert!(MAP_SIZE == 2 * 1024 * 1024);
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

    fn claim_word_bit(self) -> (usize, u64) {
        let index = self.get();
        let word = index / CLAIM_WORD_BITS;
        let bit = index % CLAIM_WORD_BITS;
        (word, 1_u64 << bit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    /// In this run's block span but not a block boundary (interior).
    InvalidPointer,
    /// Outside `span` (wrong run, tail slack, or claim tail).
    OutOfRange,
    DoubleFree,
}

/// Run-owned remote-admission bitmap.
///
/// Remote `claim` is `issued` + `try_set`. A second claim on the same bit is
/// `DoubleFree`. Owner `free` does not consult this map (owner DF is undefined).
/// `accept` drains bits onto the pointer freelist.
struct ClaimBits {
    /// 8-aligned claim words in the space tail.
    words: NonNull<AtomicU64>,
    word_count: usize,
}

impl ClaimBits {
    fn byte_len(capacity: usize) -> Option<usize> {
        let words = capacity.div_ceil(CLAIM_WORD_BITS);
        words.checked_mul(size_of::<u64>())
    }

    /// Byte offset of the claim span from the space base (`RUN_SIZE`, 8-aligned).
    fn space_offset() -> Option<usize> {
        RUN_SIZE.checked_next_multiple_of(size_of::<u64>())
    }

    fn word_count(capacity: usize) -> usize {
        capacity.div_ceil(CLAIM_WORD_BITS)
    }

    fn new(base: NonNull<u8>, offset: usize, capacity: usize) -> Option<Self> {
        let addr = base.as_ptr().wrapping_byte_add(offset).expose_provenance();
        if !addr.is_multiple_of(align_of::<AtomicU64>()) {
            return None;
        }
        Some(Self {
            words: NonNull::new(core::ptr::with_exposed_provenance_mut(addr))?,
            word_count: Self::word_count(capacity),
        })
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
        // SAFETY: `word < word_count`; `words` points at the claim span in this
        // run's space tail and aligned for `AtomicU64`.
        unsafe { &*self.words.as_ptr().add(word) }
    }
}

pub(crate) struct Run {
    /// Cached payload base (`RUN_SIZE` bytes) in a heap-owned map.
    base: NonNull<u8>,
    /// `capacity * stride` — payload bytes that are real blocks (≤ `RUN_SIZE`).
    span: u32,
    /// `ceil(2^32 / stride)` — exact `floor(offset / stride)` for `offset < 2^16`.
    recip: u32,
    /// Owner-local freelist / live / bump. `free` / `live` / `capacity` sit on the
    /// same line as `base` / `span` / `recip` so `Run::free` is one dependent line
    /// after `current[class]` (`repr(Rust)` is not ABI).
    state: UnsafeCell<RunState>,
    stride: usize,
    claims: ClaimBits,
    class: SizeClass,
    id: RunId,
    heap: HeapId,
    policy: RunPolicy,
    /// Mirror of `RunState.bump` for remote `claim`. Cold.
    issued: AtomicUsize,
    /// Coalesced-by-run inbox membership (see `heap::inbox`). Cold.
    link: InboxLink<Run>,
}

// SAFETY: owner-local methods are called only by the owning heap. Remote methods only touch
// the claim bitmap / `InboxLink`, load `issued`, and never mutate `RunState`
// (except `accept`, itself an owner-local method called only through the owning heap's flush).
unsafe impl Sync for Run {}

impl InboxNode for Run {
    fn link(&self) -> &InboxLink<Self> {
        &self.link
    }
}

/// Empty freelist head / end-of-list link. Payload address `0` is never a block.
const FREE_END: usize = 0;

struct RunState {
    /// `FREE_END` or a payload address of a free block.
    free: usize,
    live: usize,
    capacity: usize,
    bump: usize,
    available_next: Option<NonNull<Run>>,
    /// On this class's `RunHeap` available list. `push_available` is a no-op when set.
    on_available: bool,
}

impl Run {
    pub(crate) fn new(
        id: RunId,
        heap: HeapId,
        base: NonNull<u8>,
        class: SizeClass,
        policy: RunPolicy,
    ) -> Option<Self> {
        let stride = class.size();
        let capacity = RUN_SIZE.checked_div(stride).filter(|&count| count > 0)?;
        let claim_bytes = ClaimBits::byte_len(capacity)?;
        let claim_offset = ClaimBits::space_offset()?;
        let need = claim_offset.checked_add(claim_bytes)?;
        if RUN_SPACE < need {
            return None;
        }
        if base.as_ptr().addr() & (RUN_SIZE - 1) != 0 {
            return None;
        }

        let claims = ClaimBits::new(base, claim_offset, capacity)?;
        debug_assert!(stride >= size_of::<usize>());
        let span = u32::try_from(capacity.checked_mul(stride)?).ok()?;
        Some(Self {
            state: UnsafeCell::new(RunState::new(capacity)),
            base,
            span,
            recip: Self::recip(u32::try_from(stride).ok()?)?,
            stride,
            claims,
            class,
            id,
            heap,
            policy,
            issued: AtomicUsize::new(0),
            link: InboxLink::new(),
        })
    }

    /// `ceil(2^32 / stride)` — exact `floor(offset / stride)` for `offset < 2^16`.
    fn recip(stride: u32) -> Option<u32> {
        u32::try_from((1_u64 << 32).div_ceil(u64::from(stride))).ok()
    }

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
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &*self.state.get() };
        state.live == state.capacity
    }

    /// Outstanding blocks on this run (allocated or remote-claimed).
    pub(crate) fn is_live(&self) -> bool {
        // SAFETY: read under owner-local access or table-locked reclaim.
        unsafe { &*self.state.get() }.live != 0
    }

    pub(crate) fn is_available(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        unsafe { &*self.state.get() }.on_available
    }

    /// Link onto the available list. Caller already checked `!is_available()`.
    pub(crate) fn link_available(&self, next: Option<NonNull<Run>>) {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        debug_assert!(!state.on_available);
        state.available_next = next;
        state.on_available = true;
    }

    /// Unlink from the available list. Returns the previous successor.
    pub(crate) fn unlink_available(&self) -> Option<NonNull<Run>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        debug_assert!(state.on_available);
        state.on_available = false;
        state.available_next.take()
    }

    pub(crate) fn range(&self) -> AddressRange {
        AddressRange::new(self.base, RUN_SIZE)
    }

    /// Hit: pop one block from the pointer freelist. Empty → caller `extend`.
    #[inline]
    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let ptr = Self::pop_free(state)?;
        debug_assert!(state.live < state.capacity);
        state.live += 1;
        Some(ptr)
    }

    /// Thread one page of fresh blocks (at least 32, or remaining) onto the freelist.
    ///
    /// `issued` advances once. Returns `false` when no fresh blocks remain.
    #[inline(never)]
    pub(crate) fn extend(&self) -> bool {
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        if state.bump >= state.capacity {
            return false;
        }
        let page_worth = PAGE_SIZE / self.stride;
        let n = page_worth.max(32).min(state.capacity - state.bump);
        if n == 0 {
            return false;
        }
        let start = state.bump;
        let end = start + n;
        for index in start..end - 1 {
            Self::write_link(
                self.address(BlockIndex::new(index)),
                self.address(BlockIndex::new(index + 1)).as_ptr().addr(),
            );
        }
        Self::write_link(self.address(BlockIndex::new(end - 1)), state.free);
        state.free = self.address(BlockIndex::new(start)).as_ptr().addr();
        state.bump = end;
        self.issued.store(end, Ordering::Relaxed);
        true
    }

    /// Owner-local: live → pointer freelist. `Ok(true)` when the run was full.
    ///
    /// Owner double-free is undefined. Remote admission is `claim` / `accept`.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<bool, RunError> {
        let block = self.locate(ptr)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        let was_full = state.live == state.capacity;
        debug_assert!(state.live > 0);
        state.live -= 1;
        Self::push_free(state, block.ptr());
        if state.live == 0 && self.policy == RunPolicy::Discard {
            self.maybe_discard(state);
        }
        Ok(was_full)
    }

    /// Freer: reserve remote admission before publish / payload reuse.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.locate(ptr)?;
        if block.index().get() >= self.issued.load(Ordering::Relaxed) {
            return Err(RunError::DoubleFree);
        }

        if !self.claims.try_set(block.index()) {
            return Err(RunError::DoubleFree);
        }
        Ok(())
    }

    /// Owner: clear inbox queued, drain every claimed bit, publish blocks to the freelist.
    ///
    /// Wakeup proof (idle-first + recheck): clears queued *before* scanning, so a racing
    /// `claim` + `Inbox::push` may re-queue the run once it is dequeued. Returns `true` when
    /// claim bits remain after the scan — the caller must `Inbox::push` again (or a racer
    /// already did). Exactly one of those pushes keeps the run queued when work remains.
    pub(crate) fn accept(&self) -> bool {
        self.link.clear_queued();

        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &mut *self.state.get() };
        for word in 0..self.claims.word_count {
            let mut bits = self.claims.drain_word(word);
            while bits != 0 {
                // `trailing_zeros` of a nonzero `u64` is always < 64, so this never truncates.
                let bit = usize::try_from(bits.trailing_zeros()).unwrap();
                bits &= bits - 1;
                let index = BlockIndex::new(word * CLAIM_WORD_BITS + bit);
                debug_assert!(index.get() < state.capacity);
                debug_assert!(state.live > 0);
                state.live -= 1;
                Self::push_free(state, self.address(index));
            }
        }

        if state.live == 0 && self.policy == RunPolicy::Discard {
            self.maybe_discard(state);
        }
        self.claims.any_set()
    }

    #[cold]
    fn maybe_discard(&self, state: &mut RunState) {
        debug_assert_eq!(self.policy, RunPolicy::Discard);
        debug_assert_eq!(state.live, 0);
        state.bump = 0;
        state.free = FREE_END;
        self.issued.store(0, Ordering::Relaxed);
        OsMemory::discard(self.range());
    }

    pub(crate) fn allocated(&self, ptr: NonNull<u8>) -> Result<Block, RunError> {
        let block = self.locate(ptr)?;
        // SAFETY: owner-local methods are called only by the owning heap.
        let state = unsafe { &*self.state.get() };
        if block.index().get() >= state.bump {
            return Err(RunError::DoubleFree);
        }
        if self.claims.is_set(block.index()) {
            return Err(RunError::DoubleFree);
        }
        Ok(block)
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
    pub(crate) fn locate(&self, ptr: NonNull<u8>) -> Result<Block, RunError> {
        let offset = u64::try_from(ptr.as_ptr().addr().wrapping_sub(self.base.as_ptr().addr()))
            .unwrap_or(u64::MAX);
        if offset >= u64::from(self.span) {
            return Err(RunError::OutOfRange);
        }
        // `recip = ceil(2^32 / stride)` is exact for `offset < 2^16`, `stride ≤ 2^15`.
        // `stride | offset` iff the low 32 bits of `offset * recip` are `< recip`.
        let product = offset.wrapping_mul(u64::from(self.recip));
        // Keep the truncation in place — a helper does not inline on this hit.
        #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
        let remainder = product as u32;
        if remainder >= self.recip {
            return Err(RunError::InvalidPointer);
        }
        let index = product >> 32;
        Ok(Block::new(
            BlockIndex::new(usize::try_from(index).map_err(|_| RunError::InvalidPointer)?),
            ptr,
        ))
    }

    /// Payload pointer for a freelist or extend index in `0..capacity`.
    #[inline]
    fn address(&self, index: BlockIndex) -> NonNull<u8> {
        // SAFETY: owner-local methods are called only by the owning heap.
        debug_assert!(index.get() < unsafe { &*self.state.get() }.capacity);
        let byte_offset = index.get() * self.stride;
        // SAFETY: freelist / `extend` only yield `index < capacity`, so
        // `byte_offset < RUN_SIZE` inside the payload span.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().add(byte_offset)) }
    }

    #[inline]
    fn pop_free(state: &mut RunState) -> Option<NonNull<u8>> {
        let raw = state.free;
        if raw == FREE_END {
            return None;
        }
        let ptr = NonNull::new(core::ptr::without_provenance_mut(raw))?;
        state.free = Self::read_link(ptr);
        Some(ptr)
    }

    /// Push using the payload pointer already proven by `locate` / `address`.
    #[inline]
    fn push_free(state: &mut RunState, ptr: NonNull<u8>) {
        Self::write_link(ptr, state.free);
        state.free = ptr.as_ptr().addr();
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
    fn new(capacity: usize) -> Self {
        Self {
            live: 0,
            capacity,
            bump: 0,
            available_next: None,
            free: FREE_END,
            on_available: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use core::ops::Deref;

    use crate::{
        layout::LayoutSpec,
        memory::{Mapping, OsMemory},
        size_class::SizeClasses,
    };

    use super::config::RunPolicy;

    use super::*;

    struct TestRun {
        run: Run,
        _map: Mapping,
    }

    impl Deref for TestRun {
        type Target = Run;

        fn deref(&self) -> &Run {
            &self.run
        }
    }

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    fn class_id(size: usize, align: usize) -> SizeClass {
        SizeClasses::class_for(layout_spec(size, align)).unwrap()
    }

    fn alloc_block(run: &Run) -> Option<NonNull<u8>> {
        run.allocate().or_else(|| {
            run.extend();
            run.allocate()
        })
    }

    fn test_heap_id() -> HeapId {
        HeapId::new(0, NonZeroU32::MIN).unwrap()
    }

    fn test_run(index: u32, class: SizeClass) -> TestRun {
        test_run_with(index, class, RunPolicy::Keep)
    }

    fn test_run_discard(index: u32, class: SizeClass) -> TestRun {
        test_run_with(index, class, RunPolicy::Discard)
    }

    fn test_run_with(index: u32, class: SizeClass, policy: RunPolicy) -> TestRun {
        let map = OsMemory::map_aligned(RUN_SPACE, RUN_SIZE).unwrap();
        let run = Run::new(
            RunId::from_index(index).unwrap(),
            test_heap_id(),
            map.base(),
            class,
            policy,
        )
        .expect("test run");
        TestRun { run, _map: map }
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let class = class_id(64, 8);
        let run = test_run(0, class);
        let capacity = RUN_SIZE / class.size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = alloc_block(&run).unwrap();
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
    fn extend_threads_fresh_blocks_onto_freelist() {
        let class = class_id(64, 8);
        let run = test_run(21, class);
        assert!(run.allocate().is_none());
        assert!(run.extend());
        let first = run.allocate().unwrap();
        assert_eq!(first, run.range().base());
        let page_worth = PAGE_SIZE / class.size();
        for _ in 1..page_worth.max(32) {
            assert!(run.allocate().is_some());
        }
        assert!(run.allocate().is_none());
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    #[test]
    fn reusable_run_reuses_returned_block() {
        let class = class_id(128, 8);
        let run = test_run(1, class);

        let ptr = alloc_block(&run).unwrap();

        assert!(run.free(ptr).is_ok());

        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let class = class_id(64, 8);
        let run = test_run(7, class);
        let new = layout_spec(64, 8);
        let ptr = alloc_block(&run).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let class = class_id(64, 8);
        let run = test_run(8, class);
        let new = layout_spec(80, 8);
        let ptr = alloc_block(&run).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    #[test]
    fn recip_matches_index_of_for_all_classes() {
        for &size in &SizeClasses::SIZES {
            let stride = u32::try_from(size).unwrap();
            let recip = Run::recip(stride).unwrap();
            let span = (RUN_SIZE / size) * size;
            for offset in 0..span {
                let product = u64::try_from(offset)
                    .unwrap()
                    .wrapping_mul(u64::from(recip));
                #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
                let divisible = (product as u32) < recip;
                assert_eq!(
                    divisible,
                    offset.is_multiple_of(size),
                    "divisibility size={size} offset={offset}"
                );
                let index = product >> 32;
                let ok = index.wrapping_mul(u64::try_from(size).unwrap())
                    == u64::try_from(offset).unwrap();
                assert_eq!(ok, divisible, "product size={size} offset={offset}");
                let class = class_id(size, 8);
                assert_eq!(
                    ok.then_some(usize::try_from(index).unwrap()),
                    class.index_of(offset),
                    "size={size} offset={offset}"
                );
            }
        }
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let class = class_id(64, 8);
        let run = test_run(2, class);
        let ptr = alloc_block(&run).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert_eq!(run.locate(interior), Err(RunError::InvalidPointer));
    }

    #[test]
    fn reusable_run_locate_covers_all_classes_boundaries_and_tail_slack() {
        for (run_index, &size) in SizeClasses::SIZES.iter().enumerate() {
            let class = class_id(size, 8);
            let run = test_run(u32::try_from(run_index).unwrap(), class);
            let capacity = RUN_SIZE / size;

            let first = alloc_block(&run).unwrap();
            assert!(run.locate(first).is_ok(), "size={size}");
            assert_eq!(
                run.locate(unsafe { NonNull::new_unchecked(first.as_ptr().add(1)) }),
                Err(RunError::InvalidPointer),
                "size={size}"
            );

            let slack_offset = capacity * size;
            if slack_offset < RUN_SIZE {
                let slack = unsafe {
                    NonNull::new_unchecked(run.range().base().as_ptr().add(slack_offset))
                };
                assert_eq!(
                    run.locate(slack),
                    Err(RunError::OutOfRange),
                    "size={size} slack"
                );
            }
        }
    }

    #[test]
    fn reusable_run_rejects_interior_pointer_for_non_power_of_two_class() {
        let class = class_id(24, 8);
        let run = test_run(2, class);
        let ptr = alloc_block(&run).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.locate(ptr).is_ok());
        assert_eq!(run.locate(interior), Err(RunError::InvalidPointer));
    }

    #[test]
    fn reusable_run_round_trips_hotspot_non_power_of_two_classes() {
        for (run_index, size) in [80, 96].into_iter().enumerate() {
            let class = class_id(size, 8);
            let run = test_run(u32::try_from(run_index).unwrap(), class);
            let ptr = alloc_block(&run).unwrap();

            assert!(run.locate(ptr).is_ok(), "size={size}");
            assert!(run.free(ptr).is_ok(), "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_rejects_claim_tail() {
        let class = class_id(64, 8);
        let run = test_run(3, class);
        let claim_tail =
            unsafe { NonNull::new_unchecked(run.range().base().as_ptr().add(RUN_SIZE)) };

        assert_eq!(run.locate(claim_tail), Err(RunError::OutOfRange));
    }

    #[test]
    fn reusable_run_rejects_foreign_run_same_offset() {
        let class = class_id(64, 8);
        let run_a = test_run(4, class);
        let run_b = test_run(5, class);
        let ptr = alloc_block(&run_b).unwrap();

        assert!(run_b.locate(ptr).is_ok());
        assert_eq!(run_a.locate(ptr), Err(RunError::OutOfRange));
    }

    #[test]
    fn reusable_run_rejects_aligned_tail_slack() {
        for (run_index, size) in [80, 96].into_iter().enumerate() {
            let class = class_id(size, 8);
            let run = test_run(u32::try_from(run_index).unwrap(), class);
            let capacity = RUN_SIZE / class.size();
            let slack_offset = capacity * class.size();
            assert!(slack_offset < RUN_SIZE, "size={size}");
            let slack =
                unsafe { NonNull::new_unchecked(run.range().base().as_ptr().add(slack_offset)) };

            assert_eq!(run.locate(slack), Err(RunError::OutOfRange), "size={size}");
        }
    }

    #[test]
    fn claim_run_reports_duplicate_remote_free() {
        let class = class_id(64, 8);
        let run = test_run(9, class);
        let ptr = alloc_block(&run).unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn claim_run_completes_to_reusable() {
        let class = class_id(64, 8);
        let run = test_run(11, class);
        let ptr = alloc_block(&run).unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert!(!run.accept());
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn accept_without_any_claim_is_a_noop() {
        let class = class_id(64, 8);
        let run = test_run(16, class);
        let ptr = alloc_block(&run).unwrap();
        assert!(!run.accept());
        // `ptr`'s block is still live (never claimed), so the next allocate is fresh.
        assert_ne!(alloc_block(&run).unwrap(), ptr);
    }

    #[test]
    fn claim_accept_works_for_all_size_classes() {
        for (run_index, &size) in SizeClasses::SIZES.iter().enumerate() {
            let class = class_id(size, 8);
            let run = test_run(u32::try_from(run_index).unwrap(), class);
            let ptr = alloc_block(&run).unwrap();
            assert_eq!(run.claim(ptr), Ok(()), "size={size}");
            assert!(!run.accept(), "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let class = class_id(17, 16);
        let run = test_run(3, class);
        let capacity = RUN_SIZE / class.size();

        for _ in 0..capacity {
            let ptr = alloc_block(&run).unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn run_range_reports_payload_span() {
        let class = class_id(8, 8);
        let run = test_run(5, class);
        let base = run.range().base();

        assert_eq!(run.range().base(), base);
        assert_eq!(run.range().len(), RUN_SIZE);
    }

    #[test]
    fn try_queue_wins_once_until_cleared() {
        use super::super::inbox::Inbox;

        let class = class_id(64, 8);
        let run = test_run(17, class);
        let a = alloc_block(&run).unwrap();
        let b = alloc_block(&run).unwrap();
        let inbox: Inbox<Run> = Inbox::new();
        let run_ptr = NonNull::from(&*run);

        assert_eq!(run.claim(a), Ok(()));
        // First claim on an idle run wins the queue race and must push.
        assert!(inbox.push(run_ptr));

        assert_eq!(run.claim(b), Ok(()));
        // A second claim while still queued must not push again.
        assert!(!inbox.push(run_ptr));

        // accept coalesces both claims from the single queued entry.
        let _ = inbox.drain();
        assert!(!run.accept());
        assert_eq!(run.allocate(), Some(b));
        assert_eq!(run.allocate(), Some(a));

        // Cleared by accept: a fresh claim can queue again.
        assert_eq!(run.claim(a), Ok(()));
        assert!(inbox.push(run_ptr));
    }

    /// Faithful simulation of the real `Heap::flush` loop: a freer claims and
    /// pushes concurrently with an "owner" that drains the inbox and re-pushes
    /// when `accept` returns true. No claim may ever be stranded (wakeup proof).
    #[test]
    fn accept_wakeup_proof_no_claim_is_ever_stranded() {
        use core::sync::atomic::AtomicBool;

        use super::super::inbox::Inbox;

        let class = class_id(64, 8);
        let run = test_run(20, class);
        let capacity = RUN_SIZE / class.size();
        // Addresses, not `NonNull<u8>`: a raw-pointer `Vec` is not `Sync`, and this slice
        // only ever crosses the thread boundary by shared reference below.
        let addrs: Vec<usize> = (0..capacity)
            .map(|_| alloc_block(&run).unwrap().as_ptr() as usize)
            .collect();
        let inbox: Inbox<Run> = Inbox::new();
        let done = AtomicBool::new(false);
        let run_ref: &Run = &run;

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let run_ptr = NonNull::from(run_ref);
                for &addr in &addrs {
                    // SAFETY: addr is one of this run's own blocks, allocated above.
                    let ptr = NonNull::new(addr as *mut u8).unwrap();
                    run_ref.claim(ptr).unwrap();
                    let _ = inbox.push(run_ptr);
                }
                done.store(true, Ordering::Release);
            });

            let mut spins = 0u32;
            loop {
                let finished = done.load(Ordering::Acquire);
                while let Some(chain) = inbox.drain() {
                    for r in chain {
                        // SAFETY: `r` is `run_ptr`, live for the scope of this test.
                        if unsafe { r.as_ref() }.accept() {
                            let _ = inbox.push(r);
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

        assert!(!run.is_live());
    }

    #[test]
    fn discard_empty_run_resets_then_extend_reuses() {
        let class = class_id(64, 8);
        let run = test_run_discard(30, class);
        let ptr = alloc_block(&run).unwrap();
        assert_eq!(run.free(ptr), Ok(false));
        assert!(!run.is_live());
        assert!(run.allocate().is_none());
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    #[test]
    fn keep_empty_run_leaves_freelist() {
        let class = class_id(64, 8);
        let run = test_run(31, class);
        let ptr = alloc_block(&run).unwrap();
        assert_eq!(run.free(ptr), Ok(false));
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn discard_after_accept_resets() {
        let class = class_id(64, 8);
        let run = test_run_discard(32, class);
        let ptr = alloc_block(&run).unwrap();
        assert_eq!(run.claim(ptr), Ok(()));
        assert!(!run.accept());
        assert!(!run.is_live());
        assert!(run.allocate().is_none());
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    #[test]
    fn discard_does_not_unmap_space() {
        let class = class_id(64, 8);
        let run = test_run_discard(33, class);
        let base = run.range().base();
        let ptr = alloc_block(&run).unwrap();
        assert_eq!(run.free(ptr), Ok(false));
        // SAFETY: space stays mapped; DONTNEED may zero the page.
        unsafe {
            base.as_ptr().write(0x11);
            assert_eq!(base.as_ptr().read(), 0x11);
        }
    }
}
