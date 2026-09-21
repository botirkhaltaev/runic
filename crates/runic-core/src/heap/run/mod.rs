use core::{
    cell::Cell,
    mem::{align_of, offset_of, size_of},
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

pub(crate) mod config;
pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, OsMemory, PAGE_SIZE},
    size_class::SizeClass,
};

use super::{
    Heap,
    inbox::{Link, Node},
};

use config::RunPolicy;

pub(crate) use heap::RunHeap;

pub(crate) const RUN_SIZE: usize = 64 * 1024;
/// Payload plus claim tail, `RUN_SIZE`-aligned.
pub(crate) const RUN_SPACE: usize = RUN_SIZE * 2;
/// Runs per heap-owned payload map.
pub(crate) const MAP_RUNS: usize = 16;
pub(crate) const MAP_SIZE: usize = MAP_RUNS * RUN_SPACE;

const _: () = assert!(MAP_SIZE == 2 * 1024 * 1024);
/// Bits per claim-bitmap word (`AtomicU64`).
const CLAIM_WORD_BITS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunId {
    index: NonZeroU32,
}

impl RunId {
    pub(crate) fn from_index(index: u32) -> Option<Self> {
        Some(Self {
            index: NonZeroU32::new(index.checked_add(1)?)?,
        })
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunFree {
    Unchanged,
    Available,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Accept {
    Done,
    Requeue,
}

/// Run-owned remote-admission bitmap.
///
/// Remote `claim` is `issued` + `try_set`. A second claim on the same bit is
/// `DoubleFree`. Owner `free` does not consult this map (owner DF is undefined).
/// `accept` drains bits onto the pointer freelist.
struct ClaimBits {
    /// 8-aligned claim words in the space tail.
    words: NonNull<[AtomicU64]>,
}

impl ClaimBits {
    fn byte_len(capacity: usize) -> Option<usize> {
        let words = capacity.div_ceil(CLAIM_WORD_BITS);
        words.checked_mul(size_of::<u64>())
    }

    /// Byte offset of the claim span from the space base (after the in-page header).
    fn space_offset() -> Option<usize> {
        RUN_SIZE
            .checked_add(size_of::<Run>())?
            .checked_next_multiple_of(align_of::<AtomicU64>())
    }

    fn word_count(capacity: usize) -> usize {
        capacity.div_ceil(CLAIM_WORD_BITS)
    }

    fn new(base: NonNull<u8>, offset: usize, capacity: usize) -> Option<Self> {
        let addr = base.as_ptr().wrapping_byte_add(offset).expose_provenance();
        if !addr.is_multiple_of(align_of::<AtomicU64>()) {
            return None;
        }
        let words = NonNull::new(core::ptr::with_exposed_provenance_mut(addr))?;
        Some(Self {
            words: NonNull::slice_from_raw_parts(words, Self::word_count(capacity)),
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
        (0..self.words.len()).any(|word| self.word_unchecked(word).load(Ordering::Acquire) != 0)
    }

    fn word_unchecked(&self, word: usize) -> &AtomicU64 {
        debug_assert!(word < self.words.len());
        // SAFETY: `word < words.len()`; `words` points at the claim span in this
        // run's space tail and aligned for `AtomicU64`.
        unsafe { &*self.words.as_ptr().cast::<AtomicU64>().add(word) }
    }
}

/// In-page header at `base + RUN_SIZE`. Owner hit packs `base`/`span`/`recip`
/// next to `RunState` (`free`/`live` first). Remote `issued`/`link`/`claims`
/// start on the next 64-byte line.
#[repr(C, align(64))]
pub(crate) struct Run {
    /// Cached payload base (`RUN_SIZE` bytes) in a heap-owned map.
    base: NonNull<u8>,
    /// `capacity * stride` — payload bytes that are real blocks (≤ `RUN_SIZE`).
    span: u32,
    /// `ceil(2^32 / stride)` — exact `floor(offset / stride)` for `offset < 2^16`.
    recip: u32,
    state: RunState,
    stride: usize,
    class: SizeClass,
    id: RunId,
    heap: &'static Heap,
    policy: RunPolicy,
    remote: RemoteLine,
}

impl PartialEq for Run {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

impl Eq for Run {}

#[repr(C, align(64))]
struct RemoteLine {
    /// Mirror of `RunState.bump` for remote `claim`. Off the owner hit line.
    issued: AtomicUsize,
    link: Link<Run>,
    claims: ClaimBits,
}

// SAFETY: owner-local methods (`allocate` / `free` / `extend` / `accept` / available-list
// membership) run only on the owning thread (or under `HeapInner`). Remote-safe surface is
// `locate`, `claim`, `link`, `heap`, `class`, `range`, `header_of`, and `resize_in_place`
// (which reads `issued`, not `RunState` Cells). Every `Cell` reader is owner-or-locked.
unsafe impl Send for Run {}
// SAFETY: same remote-safe surface as `Send`; shared access is atomic (`issued` / `link` /
// claims) or immutable after publication (`base`, `span`, `recip`, `heap`).
unsafe impl Sync for Run {}

const _: () = assert!(offset_of!(Run, state) == 16);
const _: () = assert!(offset_of!(Run, remote) % 64 == 0);
const _: () = assert!(RUN_SPACE >= RUN_SIZE + size_of::<Run>() + (RUN_SIZE / 8).div_ceil(64) * 8);

impl Node for Run {
    fn link(&self) -> &Link<Self> {
        &self.remote.link
    }
}

/// Empty freelist head / end-of-list link. Payload address `0` is never a block.
const FREE_END: usize = 0;

/// Intrusive membership on this class's `RunHeap` available list.
///
/// `Unlisted` is off the list. `Tail` is listed with no successor — the same `None`
/// next pointer as `Unlisted`, which is why membership is not a separate bool.
#[derive(Clone, Copy)]
enum AvailableLink {
    Unlisted,
    Tail,
    Next(&'static Run),
}

impl AvailableLink {
    fn is_unlisted(self) -> bool {
        matches!(self, Self::Unlisted)
    }

    fn from_next(next: Option<&'static Run>) -> Self {
        match next {
            None => Self::Tail,
            Some(run) => Self::Next(run),
        }
    }

    fn successor(self) -> Option<&'static Run> {
        match self {
            Self::Unlisted | Self::Tail => None,
            Self::Next(run) => Some(run),
        }
    }
}

struct RunState {
    /// `FREE_END` or a payload address of a free block.
    free: Cell<usize>,
    live: Cell<usize>,
    capacity: usize,
    bump: Cell<usize>,
    available: Cell<AvailableLink>,
}

impl Run {
    pub(crate) fn new(
        id: RunId,
        heap: &'static Heap,
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
            base,
            span,
            recip: Self::recip(u32::try_from(stride).ok()?)?,
            state: RunState::new(capacity),
            stride,
            class,
            id,
            heap,
            policy,
            remote: RemoteLine {
                issued: AtomicUsize::new(0),
                link: Link::new(),
                claims,
            },
        })
    }

    /// `ceil(2^32 / stride)` — exact `floor(offset / stride)` for `offset < 2^16`.
    fn recip(stride: u32) -> Option<u32> {
        u32::try_from((1_u64 << 32).div_ceil(u64::from(stride))).ok()
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) const fn heap(&self) -> &'static Heap {
        self.heap
    }

    /// In-page header at `(ptr & !(RUN_SIZE-1)) + RUN_SIZE`. Self-check `base`.
    #[inline]
    pub(crate) fn header_of(ptr: NonNull<u8>) -> Option<&'static Self> {
        let masked = ptr.as_ptr().addr() & !(RUN_SIZE - 1);
        let header = masked.wrapping_add(RUN_SIZE);
        let base_ptr = core::ptr::with_exposed_provenance::<usize>(header);
        // SAFETY: run spaces map this aligned address and `base` is the first
        // `repr(C)` field. Reading it as `usize` is valid even for a zeroed,
        // unused run slot; only a matching initialized header is returned.
        if unsafe { base_ptr.read() } != masked {
            return None;
        }
        // SAFETY: `header` is `RUN_SIZE`-aligned and the raw base word matched
        // this run. Run mappings stay live for the process.
        Some(unsafe { &*core::ptr::with_exposed_provenance::<Self>(header) })
    }

    /// Clear the in-page `base` word so [`Self::header_of`] fails closed.
    pub(super) fn poison(&self) {
        // SAFETY: unpublished header, exclusive to the constructing `RunHeap`.
        unsafe {
            core::ptr::from_ref(self)
                .cast::<usize>()
                .cast_mut()
                .write(0);
        }
    }

    pub(crate) const fn class(&self) -> SizeClass {
        self.class
    }

    /// True when every block is outstanding (allocated or remote-claimed).
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.state.live.get() == self.state.capacity
    }

    /// Outstanding blocks on this run (allocated or remote-claimed).
    pub(crate) fn is_live(&self) -> bool {
        self.state.live.get() != 0
    }

    /// Empty Discard payload: [`Self::discard`] returns it to the OS. Keep retains.
    pub(crate) fn is_discardable(&self) -> bool {
        self.policy == RunPolicy::Discard && !self.is_live()
    }

    pub(super) fn listed(&self) -> bool {
        !self.state.available.get().is_unlisted()
    }

    /// Link onto the available list. Caller already checked `!listed()`.
    pub(super) fn list_available(&self, next: Option<&'static Run>) {
        debug_assert!(!self.listed());
        self.state.available.set(AvailableLink::from_next(next));
    }

    /// Unlink from the available list. Returns the previous successor.
    pub(super) fn unlist_available(&self) -> Option<&'static Run> {
        debug_assert!(self.listed());
        self.state
            .available
            .replace(AvailableLink::Unlisted)
            .successor()
    }

    pub(crate) fn range(&self) -> AddressRange {
        AddressRange::new(self.base, RUN_SIZE)
    }

    /// Hit: pop one block from the pointer freelist. Empty → caller `extend`.
    #[inline]
    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        let ptr = Self::pop_free(&self.state)?;
        let live = self.state.live.get();
        debug_assert!(live < self.state.capacity);
        if live == 0 {
            self.add_live();
        }
        self.state.live.set(live + 1);
        Some(ptr)
    }

    /// Thread one page of fresh blocks (at least 32, or remaining) onto the freelist.
    ///
    /// `issued` advances once. Returns `false` when no fresh blocks remain.
    #[inline(never)]
    pub(crate) fn extend(&self) -> bool {
        let bump = self.state.bump.get();
        if bump >= self.state.capacity {
            return false;
        }
        let page_worth = PAGE_SIZE / self.stride;
        let n = page_worth.max(32).min(self.state.capacity - bump);
        if n == 0 {
            return false;
        }
        let end = bump + n;
        for index in bump..end - 1 {
            Self::write_link(
                self.address(BlockIndex::new(index)),
                self.address(BlockIndex::new(index + 1)).as_ptr().addr(),
            );
        }
        Self::write_link(
            self.address(BlockIndex::new(end - 1)),
            self.state.free.get(),
        );
        self.state
            .free
            .set(self.address(BlockIndex::new(bump)).as_ptr().addr());
        self.state.bump.set(end);
        self.remote.issued.store(end, Ordering::Relaxed);
        true
    }

    /// Owner-local: live → pointer freelist. `Available` when the run was full.
    ///
    /// Hit ignores the outcome and does not discard. Miss / slow / unbind call
    /// [`Self::discard`] and `push_available` from the outcome. Owner DF is
    /// undefined. Remote admission is `claim` / `accept`.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<RunFree, RunError> {
        let block = self.locate(ptr)?;
        let live = self.state.live.get();
        let was_full = live == self.state.capacity;
        debug_assert!(live > 0);
        self.state.live.set(live - 1);
        Self::push_free(&self.state, block.ptr());
        if live == 1 {
            self.sub_live();
        }
        Ok(if was_full {
            RunFree::Available
        } else {
            RunFree::Unchanged
        })
    }

    /// Freer: reserve remote admission before publish / payload reuse.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.locate(ptr)?;
        if block.index().get() >= self.remote.issued.load(Ordering::Relaxed) {
            return Err(RunError::DoubleFree);
        }

        if !self.remote.claims.try_set(block.index()) {
            return Err(RunError::DoubleFree);
        }
        Ok(())
    }

    /// Owner: clear inbox queued, drain every claimed bit, publish blocks to the freelist.
    ///
    /// Wakeup proof (idle-first + recheck): clears queued *before* scanning, so a racing
    /// `claim` + `Inbox::queue` may re-queue the run once it is dequeued. Returns `Requeue` when
    /// claim bits remain after the scan — the caller must `Inbox::queue` again (or a racer
    /// already did). Exactly one of those queues keeps the run queued when work remains.
    pub(crate) fn accept(&self) -> Accept {
        self.remote.link.clear_queued();

        let was_live = self.state.live.get() != 0;
        for word in 0..self.remote.claims.words.len() {
            let mut bits = self.remote.claims.drain_word(word);
            while bits != 0 {
                // `trailing_zeros` of a nonzero `u64` is always < 64, so this never truncates.
                let bit = usize::try_from(bits.trailing_zeros()).unwrap();
                bits &= bits - 1;
                let index = BlockIndex::new(word * CLAIM_WORD_BITS + bit);
                debug_assert!(index.get() < self.state.capacity);
                let live = self.state.live.get();
                debug_assert!(live > 0);
                self.state.live.set(live - 1);
                Self::push_free(&self.state, self.address(index));
            }
        }

        if was_live && self.state.live.get() == 0 {
            self.sub_live();
        }
        if self.is_discardable() {
            self.discard();
        }
        if self.remote.claims.any_set() {
            Accept::Requeue
        } else {
            Accept::Done
        }
    }

    fn add_live(&self) {
        self.heap.add_run_live();
    }

    fn sub_live(&self) {
        self.heap.sub_run_live();
    }

    /// `madvise` the payload and reset the run to fresh. Off the free hit.
    ///
    /// Caller checked [`Self::is_discardable`].
    #[cold]
    pub(crate) fn discard(&self) {
        debug_assert!(self.is_discardable());
        self.state.bump.set(0);
        self.state.free.set(FREE_END);
        self.remote.issued.store(0, Ordering::Relaxed);
        OsMemory::discard(self.range());
    }

    pub(crate) fn allocated(&self, ptr: NonNull<u8>) -> Result<Block, RunError> {
        let block = self.locate(ptr)?;
        if block.index().get() >= self.remote.issued.load(Ordering::Acquire) {
            return Err(RunError::DoubleFree);
        }
        if self.remote.claims.is_set(block.index()) {
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
        if product & u64::from(u32::MAX) >= u64::from(self.recip) {
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
        debug_assert!(index.get() < self.state.capacity);
        let byte_offset = index.get() * self.stride;
        // SAFETY: freelist / `extend` only yield `index < capacity`, so
        // `byte_offset < RUN_SIZE` inside the payload span.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().add(byte_offset)) }
    }

    #[inline]
    fn pop_free(state: &RunState) -> Option<NonNull<u8>> {
        let raw = state.free.get();
        if raw == FREE_END {
            return None;
        }
        let ptr = NonNull::new(core::ptr::without_provenance_mut(raw))?;
        state.free.set(Self::read_link(ptr));
        Some(ptr)
    }

    /// Push using the payload pointer already proven by `locate` / `address`.
    #[inline]
    fn push_free(state: &RunState, ptr: NonNull<u8>) {
        Self::write_link(ptr, state.free.get());
        state.free.set(ptr.as_ptr().addr());
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
            live: Cell::new(0),
            capacity,
            bump: Cell::new(0),
            available: Cell::new(AvailableLink::Unlisted),
            free: Cell::new(FREE_END),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use crate::{
        config::AllocatorConfig,
        heap::{Heap, HeapId},
        layout::LayoutSpec,
        memory::{OsMemory, PageMap, PageOwner},
        size_class::SizeClasses,
    };

    use super::config::{RunConfig, RunPolicy};

    use super::*;

    static OWNER: Heap = Heap::new(
        HeapId::new(0, NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );

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

    #[test]
    fn run_equality_is_identity() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let first = runs.acquire(class, &OWNER, &pages).unwrap();
        let second = runs.acquire(class, &OWNER, &pages).unwrap();
        let ptr = alloc_block(first).unwrap();

        assert!(first == first);
        assert!(first != second);
        assert!(first == Run::header_of(ptr).unwrap());
    }

    #[test]
    fn header_of_rejects_zeroed_unused_map_slot() {
        let map = OsMemory::map_aligned(RUN_SPACE * 2, RUN_SIZE).unwrap();
        let unused = NonNull::new(map.base().as_ptr().wrapping_byte_add(RUN_SPACE)).unwrap();

        assert!(Run::header_of(unused).is_none());
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, &OWNER, &pages).unwrap();
        let capacity = RUN_SIZE / class.size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = alloc_block(run).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, &OWNER, &pages).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(128, 8), &OWNER, &pages).unwrap();

        let ptr = alloc_block(run).unwrap();

        assert!(run.free(ptr).is_ok());

        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let new = layout_spec(64, 8);
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let new = layout_spec(80, 8);
        let ptr = alloc_block(run).unwrap();

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
                let [b0, b1, b2, b3, _, _, _, _] = product.to_le_bytes();
                let divisible = u32::from_le_bytes([b0, b1, b2, b3]) < recip;
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
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert_eq!(run.locate(interior), Err(RunError::InvalidPointer));
    }

    #[test]
    fn reusable_run_locate_covers_all_classes_boundaries_and_tail_slack() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        for &size in &SizeClasses::SIZES {
            let run = runs.acquire(class_id(size, 8), &OWNER, &pages).unwrap();
            let capacity = RUN_SIZE / size;

            let first = alloc_block(run).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(24, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.locate(ptr).is_ok());
        assert_eq!(run.locate(interior), Err(RunError::InvalidPointer));
    }

    #[test]
    fn reusable_run_round_trips_hotspot_non_power_of_two_classes() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        for size in [80, 96] {
            let run = runs.acquire(class_id(size, 8), &OWNER, &pages).unwrap();
            let ptr = alloc_block(run).unwrap();

            assert!(run.locate(ptr).is_ok(), "size={size}");
            assert!(run.free(ptr).is_ok(), "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_rejects_claim_tail() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let claim_tail =
            unsafe { NonNull::new_unchecked(run.range().base().as_ptr().add(RUN_SIZE)) };

        assert_eq!(run.locate(claim_tail), Err(RunError::OutOfRange));
    }

    #[test]
    fn reusable_run_rejects_foreign_run_same_offset() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let base_a = runs.acquire(class, &OWNER, &pages).unwrap().range().base();
        let base_b = runs.acquire(class, &OWNER, &pages).unwrap().range().base();
        let (Some(PageOwner::Run(run_a)), Some(PageOwner::Run(run_b))) =
            (pages.get(base_a), pages.get(base_b))
        else {
            panic!("expected two published runs");
        };
        let ptr = alloc_block(run_b).unwrap();

        assert!(run_b.locate(ptr).is_ok());
        assert_eq!(run_a.locate(ptr), Err(RunError::OutOfRange));
    }

    #[test]
    fn reusable_run_rejects_aligned_tail_slack() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        for size in [80, 96] {
            let class = class_id(size, 8);
            let run = runs.acquire(class, &OWNER, &pages).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn claim_run_completes_to_reusable() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.accept(), Accept::Done);
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn accept_without_any_claim_is_a_noop() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.accept(), Accept::Done);
        // `ptr`'s block is still live (never claimed), so the next allocate is fresh.
        assert_ne!(alloc_block(run).unwrap(), ptr);
    }

    #[test]
    fn claim_accept_works_for_all_size_classes() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        for &size in &SizeClasses::SIZES {
            let run = runs.acquire(class_id(size, 8), &OWNER, &pages).unwrap();
            let ptr = alloc_block(run).unwrap();
            assert_eq!(run.claim(ptr), Ok(()), "size={size}");
            assert_eq!(run.accept(), Accept::Done, "size={size}");
            assert_eq!(run.allocate(), Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(17, 16);
        let run = runs.acquire(class, &OWNER, &pages).unwrap();
        let capacity = RUN_SIZE / class.size();

        for _ in 0..capacity {
            let ptr = alloc_block(run).unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn run_range_reports_payload_span() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(8, 8), &OWNER, &pages).unwrap();
        let base = run.range().base();

        assert_eq!(run.range().base(), base);
        assert_eq!(run.range().len(), RUN_SIZE);
    }

    #[test]
    fn try_queue_wins_once_until_cleared() {
        use super::super::inbox::Inbox;

        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let a = alloc_block(run).unwrap();
        let b = alloc_block(run).unwrap();
        let inbox: Inbox<'_, Run> = Inbox::new();

        assert_eq!(run.claim(a), Ok(()));
        // First claim on an idle run wins the queue race and must push.
        assert!(inbox.queue(run));

        assert_eq!(run.claim(b), Ok(()));
        // A second claim while still queued must not push again.
        assert!(!inbox.queue(run));

        // accept coalesces both claims from the single queued entry.
        let _ = inbox.drain();
        assert_eq!(run.accept(), Accept::Done);
        assert_eq!(run.allocate(), Some(b));
        assert_eq!(run.allocate(), Some(a));

        // Cleared by accept: a fresh claim can queue again.
        assert_eq!(run.claim(a), Ok(()));
        assert!(inbox.queue(run));
    }

    /// Faithful simulation of the real `Heap::flush` loop: a freer claims and
    /// pushes concurrently with an "owner" that drains the inbox and re-pushes
    /// when `accept` returns true. No claim may ever be stranded (wakeup proof).
    #[test]
    fn accept_wakeup_proof_no_claim_is_ever_stranded() {
        use core::sync::atomic::AtomicBool;

        use super::super::inbox::Inbox;

        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, &OWNER, &pages).unwrap();
        let capacity = RUN_SIZE / class.size();
        // Addresses, not `NonNull<u8>`: a raw-pointer `Vec` is not `Sync`, and this slice
        // only ever crosses the thread boundary by shared reference below.
        let addrs: Vec<usize> = (0..capacity)
            .map(|_| alloc_block(run).unwrap().as_ptr().expose_provenance())
            .collect();
        let inbox: Inbox<'_, Run> = Inbox::new();
        let done = AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for &addr in &addrs {
                    // SAFETY: addr is one of this run's own blocks, allocated above.
                    let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(addr)).unwrap();
                    run.claim(ptr).unwrap();
                    inbox.queue(run);
                }
                done.store(true, Ordering::Release);
            });

            let mut spins = 0u32;
            loop {
                let finished = done.load(Ordering::Acquire);
                while let Some(chain) = inbox.drain() {
                    for run in chain {
                        if run.accept() == Accept::Requeue {
                            inbox.queue(run);
                        }
                    }
                }
                if finished && inbox.is_empty() && !run.remote.claims.any_set() {
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
    fn discarded_run_resets_then_extend_reuses() {
        let mut runs = RunHeap::new(RunConfig::new().with_policy(RunPolicy::Discard));
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        assert!(run.is_discardable());
        run.discard();
        assert!(!run.is_live());
        assert!(run.allocate().is_none());
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    #[test]
    fn keep_empty_run_leaves_freelist() {
        let mut runs = RunHeap::new(RunConfig::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        assert!(!run.is_discardable());
        assert_eq!(run.allocate(), Some(ptr));
    }

    #[test]
    fn discard_after_accept_resets() {
        let mut runs = RunHeap::new(RunConfig::new().with_policy(RunPolicy::Discard));
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.accept(), Accept::Done);
        assert!(!run.is_live());
        assert!(run.allocate().is_none());
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    #[test]
    fn discard_does_not_unmap_space() {
        let mut runs = RunHeap::new(RunConfig::new().with_policy(RunPolicy::Discard));
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), &OWNER, &pages).unwrap();
        let base = run.range().base();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        run.discard();
        // SAFETY: space stays mapped; DONTNEED may zero the page.
        unsafe {
            base.as_ptr().write(0x11);
            assert_eq!(base.as_ptr().read(), 0x11);
        }
    }
}
