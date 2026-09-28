use core::{
    cell::Cell,
    mem::{offset_of, size_of},
    num::NonZeroU32,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};
#[cfg(feature = "safe")]
use core::{mem::align_of, sync::atomic::AtomicU64};

pub(crate) mod config;
mod freelist;
pub(crate) mod heap;
mod slot;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Memory, Os, PAGE_SIZE},
    size_class::SizeClass,
};

#[cfg(any(feature = "safe", feature = "hardened"))]
use crate::allocator::Allocator;
#[cfg(feature = "hardened")]
use crate::heap::checksum::Checksum;

use super::{
    Heap,
    inbox::{Link, Node},
    queue,
};

use config::RunPolicy;
pub(crate) use freelist::Freelist;
pub(crate) use heap::RunHeap;
use slot::{Canary, Slot};

pub(crate) const RUN_SIZE: usize = 64 * 1024;
/// Payload plus claim tail, `RUN_SIZE`-aligned.
pub(crate) const RUN_SPACE: usize = RUN_SIZE * 2;
/// Runs per heap-owned payload map.
pub(crate) const MAP_RUNS: usize = 16;
pub(crate) const MAP_SIZE: usize = MAP_RUNS * RUN_SPACE;

const _: () = assert!(MAP_SIZE == 2 * 1024 * 1024);
/// Bits per claim-bitmap word (`AtomicU64`). `safe` only.
#[cfg(feature = "safe")]
const CLAIM_WORD_BITS: usize = 64;
/// Claim words for the smallest block (`size_of::<usize>()`). Accept clears by word.
#[cfg(feature = "safe")]
const MAX_CLAIM_WORDS: usize = (RUN_SIZE / size_of::<usize>()).div_ceil(CLAIM_WORD_BITS);

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

    #[cfg(feature = "safe")]
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

/// Claim bits gathered while accepting one chain. Empty on Fast.
struct ClaimMask {
    #[cfg(feature = "safe")]
    words: [u64; MAX_CLAIM_WORDS],
}

impl ClaimMask {
    fn new() -> Self {
        Self {
            #[cfg(feature = "safe")]
            words: [0; MAX_CLAIM_WORDS],
        }
    }
}

/// Run-owned remote double-free bits. Compiled only for `safe`.
///
/// `claim` is `issued` plus `try_set`. A second claim on the same bit is
/// `DoubleFree`, including while the block sits on a thread slot. Fast leaves
/// that second claim undefined and does not store the map. `accept` clears the
/// bits of the chain it splices.
#[cfg(feature = "safe")]
struct ClaimBits {
    /// 8-aligned claim words in the space tail.
    words: NonNull<[AtomicU64]>,
}

#[cfg(feature = "safe")]
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

    /// Clear `mask` in `word`. Owner only, for blocks just taken off the remote chain.
    #[inline]
    fn clear_mask(&self, word: usize, mask: u64) {
        self.word_unchecked(word).fetch_and(!mask, Ordering::AcqRel);
    }

    fn word_unchecked(&self, word: usize) -> &AtomicU64 {
        debug_assert!(word < self.words.len());
        // SAFETY: `word < words.len()`. Callers pass a word from a `BlockIndex`
        // inside this run's capacity. `words` is the aligned claim span in the space tail.
        unsafe { &*self.words.as_ptr().cast::<AtomicU64>().add(word) }
    }
}

/// In-page header at `base + RUN_SIZE`. Owner hit packs `base`/`span`/`recip`
/// next to `RunState` (`free`/`live` first). Remote `issued`/`link`/`chain`
/// start on the next 64-byte line. `safe` adds claim bits on that line.
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
    /// Class, owner, and base. Checked on acquire and free.
    #[cfg(feature = "hardened")]
    checksum: Checksum,
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
    /// Claimed-block chain. Null is empty. Each block's first word is the next address.
    chain: AtomicPtr<u8>,
    #[cfg(feature = "safe")]
    claims: ClaimBits,
}

// SAFETY: owner-local methods (`allocate` / `free` / `extend` / `accept` / available-list
// membership) run only on the owner (or under `HeapInner`). Remote-safe surface is
// `locate`, `claim`, `push`, `link`, `heap`, `class`, `range`, `header_of`, and
// `resize_in_place` (which reads `issued`, not `RunState` Cells). Every `Cell` reader is
// owner-or-locked.
unsafe impl Send for Run {}
// SAFETY: same remote-safe surface as `Send`; shared access is atomic (`issued` / `link` /
// `chain`, and `safe` claim bits) or immutable after publication (`base`, `span`, `recip`, `heap`).
unsafe impl Sync for Run {}

const _: () = assert!(offset_of!(Run, state) == 16);
const _: () = assert!(offset_of!(Run, remote) % 64 == 0);
const _: () = assert!(RUN_SPACE >= RUN_SIZE + size_of::<Run>() + (RUN_SIZE / 8).div_ceil(64) * 8);

impl Node for Run {
    fn link(&self) -> &Link<Self> {
        &self.remote.link
    }
}

impl queue::Linked for Run {
    fn links(&self) -> &queue::Link<Self> {
        &self.state.available
    }
}

struct RunState {
    /// Free-block stack. Payload address, or 0 when empty.
    free: Freelist,
    live: Cell<usize>,
    capacity: usize,
    bump: Cell<usize>,
    available: queue::Link<Run>,
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
        if base.as_ptr().addr() & (RUN_SIZE - 1) != 0 {
            return None;
        }
        #[cfg(feature = "safe")]
        let claims = {
            let claim_bytes = ClaimBits::byte_len(capacity)?;
            let claim_offset = ClaimBits::space_offset()?;
            let need = claim_offset.checked_add(claim_bytes)?;
            if RUN_SPACE < need {
                return None;
            }
            ClaimBits::new(base, claim_offset, capacity)?
        };
        debug_assert!(stride >= size_of::<usize>());
        let span = u32::try_from(capacity.checked_mul(stride)?).ok()?;
        Some(Self {
            base,
            span,
            recip: Self::recip(u32::try_from(stride).ok()?)?,
            state: RunState::new(capacity, base),
            stride,
            class,
            id,
            heap,
            policy,
            remote: RemoteLine {
                issued: AtomicUsize::new(0),
                link: Link::new(),
                chain: AtomicPtr::new(ptr::null_mut()),
                #[cfg(feature = "safe")]
                claims,
            },
            #[cfg(feature = "hardened")]
            checksum: Checksum::of(Self::digest(base, class, heap)),
        })
    }

    /// The header words the checksum covers.
    #[cfg(feature = "hardened")]
    fn digest(base: NonNull<u8>, class: SizeClass, heap: &Heap) -> [usize; 3] {
        [
            base.as_ptr().addr(),
            class.index(),
            core::ptr::from_ref(heap).addr(),
        ]
    }

    /// Abort when the published header fields no longer match. No-op on Fast.
    #[inline]
    pub(crate) fn check_header(&self) {
        #[cfg(feature = "hardened")]
        if self.checksum != Checksum::of(Self::digest(self.base, self.class, self.heap)) {
            Allocator::abort();
        }
        #[cfg(not(feature = "hardened"))]
        {
            let _ = self;
        }
    }

    /// Bytes the caller may write. Hardened reserves the last word for the canary.
    #[inline]
    pub(crate) fn usable(&self) -> usize {
        let size = self.class.size();
        #[cfg(feature = "hardened")]
        {
            size.saturating_sub(size_of::<usize>())
        }
        #[cfg(not(feature = "hardened"))]
        {
            size
        }
    }

    /// Push a block that just left the delay onto this freelist.
    #[inline]
    pub(crate) fn push_free(&self, block: NonNull<u8>) {
        self.state.free.push(block);
    }

    /// Write `block`'s link with this run's cookie.
    #[inline]
    pub(crate) fn link_block(&self, block: NonNull<u8>, next: Option<NonNull<u8>>) {
        self.state.free.link(block, next);
    }

    /// Read `block`'s link. `None` is the tail.
    #[inline]
    pub(crate) fn next_block(&self, block: NonNull<u8>) -> Option<NonNull<u8>> {
        self.state.free.next(block)
    }

    /// Blocks held back for this run, once [`Self::extend`] cannot serve.
    /// Fast has nothing held back.
    #[inline]
    pub(crate) fn restock(&self) -> Option<NonNull<u8>> {
        #[cfg(feature = "hardened")]
        {
            self.heap.delay().recall(self);
            self.allocate()
        }
        #[cfg(not(feature = "hardened"))]
        {
            let _ = self;
            None
        }
    }

    /// The slot beginning at `block`. `block` is a block of this run.
    #[inline]
    fn slot(&self, block: NonNull<u8>) -> Slot {
        Slot::new(
            block,
            self.stride,
            #[cfg(feature = "hardened")]
            self.state.free.cookie(),
        )
    }

    /// Owner free of one block. Fast pushes. Hardened delays until the budget,
    /// except the block that would leave a fully issued run with an empty freelist.
    #[inline]
    fn release(&self, block: NonNull<u8>) {
        #[cfg(feature = "hardened")]
        if self.state.free.is_empty() && self.state.bump.get() == self.state.capacity {
            self.push_free(block);
        } else {
            self.heap.delay().hold(self, block);
        }
        #[cfg(not(feature = "hardened"))]
        self.push_free(block);
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
        // SAFETY: loads one aligned word at `base + RUN_SIZE`. This faults when
        // that address is not a run mapping, so pointer-only free must not call
        // `header_of`. A zeroed unused slot reads as 0 and fails the `base` check.
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
        self.state.available.is_linked()
    }

    pub(crate) fn range(&self) -> AddressRange {
        AddressRange::new(self.base, RUN_SIZE)
    }

    /// Hit: pop one block from the pointer freelist. Empty → caller `extend`.
    #[inline]
    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        let ptr = self.state.free.pop()?;
        self.slot(ptr).mark(Canary::Live);
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
        self.state
            .free
            .push_contiguous(self.address(BlockIndex::new(bump)), n, self.stride);
        self.state.bump.set(end);
        self.remote.issued.store(end, Ordering::Relaxed);
        true
    }

    /// Owner-local: live → pointer freelist. `Available` when the run was full.
    ///
    /// Hit ignores the outcome and does not discard. Miss / slow / unbind call
    /// [`Self::discard`] and `push_available` from the outcome. Owner double-free
    /// is undefined on Fast. The `safe` feature aborts when the block is past
    /// `bump`, its claim bit is set, the address is already on this freelist, or
    /// `live` is 0. Remote admission is `claim` / `accept`.
    #[inline]
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<RunFree, RunError> {
        let block = self.locate(ptr)?;
        let live = self.state.live.get();
        self.check_header();
        let slot = self.slot(block.ptr());
        slot.expect(Canary::Live);
        slot.mark(Canary::Free);
        self.guard_owner(block, live);
        let was_full = live == self.state.capacity;
        debug_assert!(live > 0);
        self.state.live.set(live - 1);
        self.release(block.ptr());
        if live == 1 {
            self.sub_live();
        }
        Ok(if was_full {
            RunFree::Available
        } else {
            RunFree::Unchanged
        })
    }

    /// Freer: claim remote admission. Does not push or enqueue.
    ///
    /// An index past `issued` is [`RunError::DoubleFree`]. On `safe`, a second
    /// claim of an issued block is also `DoubleFree`, including while the block
    /// sits on a thread slot. Fast leaves that second claim undefined. The
    /// caller then links the block and [`Self::push`]es.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.locate(ptr)?;
        if block.index().get() >= self.remote.issued.load(Ordering::Relaxed) {
            return Err(RunError::DoubleFree);
        }
        if !self.try_claim(block.index()) {
            return Err(RunError::DoubleFree);
        }
        let slot = self.slot(ptr);
        slot.expect(Canary::Live);
        slot.mark(Canary::Claim);
        Ok(())
    }

    /// Prepend a claimed chain. `tail`'s first word becomes the previous head.
    ///
    /// Every block in `head`…`tail` was [`Self::claim`]ed by this thread. A null
    /// chain head means empty, the same end marker as [`Freelist`].
    pub(crate) fn push(&self, head: NonNull<u8>, tail: NonNull<u8>) {
        let mut current = self.remote.chain.load(Ordering::Acquire);
        loop {
            self.link_block(tail, NonNull::new(current));
            match self.remote.chain.compare_exchange(
                current,
                head.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Owner: store the inbox link idle, take the chain, and splice it.
    /// `safe` clears the claim bits of those blocks.
    ///
    /// Idle is stored before the swap, so a freer who [`Self::push`]es after the
    /// take sees an idle link and enqueues. [`Accept::Requeue`] means a push landed
    /// after the swap. [`Accept::Done`] means the chain head is still empty.
    pub(crate) fn accept(&self) -> Accept {
        self.check_header();
        self.remote.link.idle();
        let taken = self.remote.chain.swap(ptr::null_mut(), Ordering::AcqRel);
        if let Some(head) = NonNull::new(taken) {
            self.take_chain(head);
        }
        if self.remote.chain.load(Ordering::Acquire).is_null() {
            Accept::Done
        } else {
            Accept::Requeue
        }
    }

    /// Splice a taken chain onto the freelist and settle `live`.
    fn take_chain(&self, head: NonNull<u8>) {
        let mut count = 0usize;
        let mut claims = ClaimMask::new();
        let mut block = head;
        let tail = loop {
            self.record(block, &mut claims);
            count += 1;
            let next = self.next_block(block);
            let slot = self.slot(block);
            slot.expect(Canary::Claim);
            slot.mark(Canary::Free);
            self.defer(block);
            match next {
                Some(next) => block = next,
                None => break block,
            }
        };
        self.clear_claims(&claims);
        self.splice(head, tail);
        let live = self.state.live.get();
        debug_assert!(live >= count);
        let left = live.saturating_sub(count);
        self.state.live.set(left);
        if live != 0 && left == 0 {
            self.sub_live();
        }
        if self.is_discardable() {
            self.discard();
        }
    }

    /// Owner double-free. No-op on Fast, where that free is undefined.
    #[inline]
    fn guard_owner(&self, block: Block, live: usize) {
        #[cfg(feature = "safe")]
        {
            if live == 0
                || block.index().get() >= self.state.bump.get()
                || self.remote.claims.is_set(block.index())
                || self
                    .state
                    .free
                    .ensure_absent(block.ptr(), self.state.capacity, |link| {
                        self.locate(link).is_ok()
                    })
                    .is_err()
            {
                Allocator::abort();
            }
        }
        #[cfg(not(feature = "safe"))]
        {
            let _ = (self, block, live);
        }
    }

    /// Reserve the block for this claim. Fast always succeeds: a second claim
    /// is undefined there.
    #[inline]
    fn try_claim(&self, index: BlockIndex) -> bool {
        #[cfg(feature = "safe")]
        {
            self.remote.claims.try_set(index)
        }
        #[cfg(not(feature = "safe"))]
        {
            let _ = (self, index);
            true
        }
    }

    /// `true` when a remote claim already owns the block. Always `false` on Fast.
    #[inline]
    fn was_claimed(&self, index: BlockIndex) -> bool {
        #[cfg(feature = "safe")]
        {
            self.remote.claims.is_set(index)
        }
        #[cfg(not(feature = "safe"))]
        {
            let _ = (self, index);
            false
        }
    }

    /// Remember a claimed block so [`Self::clear_claims`] can drop its bit.
    fn record(&self, block: NonNull<u8>, claims: &mut ClaimMask) {
        #[cfg(feature = "safe")]
        {
            let Ok(located) = self.locate(block) else {
                Allocator::abort();
            };
            let (word, mask) = located.index().claim_word_bit();
            let Some(bits) = claims.words.get_mut(word) else {
                Allocator::abort();
            };
            *bits |= mask;
        }
        #[cfg(not(feature = "safe"))]
        {
            let _ = (self, block, claims);
        }
    }

    fn clear_claims(&self, claims: &ClaimMask) {
        #[cfg(feature = "safe")]
        for (word, mask) in claims.words.into_iter().enumerate() {
            if mask != 0 {
                self.remote.claims.clear_mask(word, mask);
            }
        }
        #[cfg(not(feature = "safe"))]
        {
            let _ = (self, claims);
        }
    }

    /// Hardened delays the block here. Fast splices the whole chain afterwards.
    #[inline]
    fn defer(&self, block: NonNull<u8>) {
        #[cfg(feature = "hardened")]
        self.release(block);
        #[cfg(not(feature = "hardened"))]
        {
            let _ = (self, block);
        }
    }

    /// Fast prepends the chain. Hardened already released each block in the walk.
    fn splice(&self, head: NonNull<u8>, tail: NonNull<u8>) {
        #[cfg(not(feature = "hardened"))]
        self.state.free.splice(head, tail);
        #[cfg(feature = "hardened")]
        {
            let _ = (self, head, tail);
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
        // Blocks still held back would return to a reset run and be issued
        // twice by `extend`. Take them back before the freelist is cleared.
        #[cfg(feature = "hardened")]
        self.heap.delay().recall(self);
        self.state.bump.set(0);
        self.state.free.clear();
        self.remote.issued.store(0, Ordering::Relaxed);
        Os::discard(self.range());
    }

    pub(crate) fn allocated(&self, ptr: NonNull<u8>) -> Result<Block, RunError> {
        let block = self.locate(ptr)?;
        if block.index().get() >= self.remote.issued.load(Ordering::Acquire) {
            return Err(RunError::DoubleFree);
        }
        if self.was_claimed(block.index()) {
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

        Ok(self.usable() >= spec.size() && spec.is_addr_aligned(ptr.as_ptr().addr()))
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
}

impl RunState {
    fn new(capacity: usize, base: NonNull<u8>) -> Self {
        Self {
            live: Cell::new(0),
            capacity,
            bump: Cell::new(0),
            available: queue::Link::new(),
            free: Freelist::new(base.as_ptr().addr()),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use crate::{
        config::{AllocatorConfig, Hints},
        heap::{Heap, HeapId},
        layout::LayoutSpec,
        memory::{PageMap, PageOwner},
        size_class::SizeClasses,
    };

    use super::config::{RunConfig, RunPolicy};

    use super::*;

    fn owner() -> &'static Heap {
        std::thread_local! {
            static SLOT: &'static Heap = Box::leak(Box::new(Heap::new(
                HeapId::new(0, NonZeroU32::MIN).unwrap(),
                AllocatorConfig::new(),
            )));
        }
        SLOT.with(|heap| *heap)
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

    #[test]
    fn run_equality_is_identity() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let first = runs.acquire(class, owner(), &pages).unwrap();
        let second = runs.acquire(class, owner(), &pages).unwrap();
        let ptr = alloc_block(first).unwrap();

        assert!(first == first);
        assert!(first != second);
        assert!(first == Run::header_of(ptr).unwrap());
    }

    #[test]
    fn header_of_rejects_zeroed_unused_map_slot() {
        let map = Os::map_aligned(RUN_SPACE * 2, RUN_SIZE).unwrap();
        let unused = NonNull::new(map.base().as_ptr().wrapping_byte_add(RUN_SPACE)).unwrap();

        assert!(Run::header_of(unused).is_none());
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, owner(), &pages).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, owner(), &pages).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(128, 8), owner(), &pages).unwrap();

        let ptr = alloc_block(run).unwrap();

        assert!(run.free(ptr).is_ok());

        let again = run.allocate();
        #[cfg(not(feature = "hardened"))]
        assert_eq!(again, Some(ptr));
        #[cfg(feature = "hardened")]
        assert_ne!(again, Some(ptr));
    }

    #[test]
    fn reusable_run_resizes_block_in_place_for_same_class_layout() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let new = layout_spec(64, 8);
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(true));
    }

    #[test]
    fn reusable_run_rejects_allocated_block_that_needs_larger_class() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let new = layout_spec(80, 8);
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.resize_in_place(ptr, new), Ok(false));
    }

    /// Every byte of every class's payload: block starts locate to their
    /// index, interior bytes are invalid, the tail slack past the last whole
    /// block is out of range, and `header_of` resolves the same run throughout.
    #[test]
    fn locate_and_header_of_classify_every_payload_byte() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        for &size in &SizeClasses::SIZES {
            #[cfg(feature = "hardened")]
            let request = size - core::mem::size_of::<usize>();
            #[cfg(not(feature = "hardened"))]
            let request = size;
            let class = class_id(request, 8);
            assert_eq!(class.size(), size);
            let run = runs.acquire(class, owner(), &pages).unwrap();
            let base = run.range().base();
            let span = (RUN_SIZE / size) * size;

            for offset in 0..RUN_SIZE {
                let ptr = NonNull::new(base.as_ptr().wrapping_add(offset)).unwrap();
                let expected = if offset >= span {
                    Err(RunError::OutOfRange)
                } else if offset.is_multiple_of(size) {
                    Ok(offset / size)
                } else {
                    Err(RunError::InvalidPointer)
                };

                assert_eq!(
                    run.locate(ptr).map(|block| block.index().get()),
                    expected,
                    "size {size} offset {offset}"
                );
                assert!(
                    Run::header_of(ptr).unwrap() == run,
                    "size {size} offset {offset}"
                );
            }
        }
    }

    #[test]
    fn reusable_run_round_trips_hotspot_non_power_of_two_classes() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        for size in [80, 96] {
            let run = runs.acquire(class_id(size, 8), owner(), &pages).unwrap();
            let ptr = alloc_block(run).unwrap();

            assert!(run.locate(ptr).is_ok(), "size={size}");
            assert!(run.free(ptr).is_ok(), "size={size}");
            let again = run.allocate();
            #[cfg(not(feature = "hardened"))]
            assert_eq!(again, Some(ptr), "size={size}");
            #[cfg(feature = "hardened")]
            assert_ne!(again, Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_rejects_claim_tail() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let claim_tail = NonNull::new(run.range().base().as_ptr().wrapping_add(RUN_SIZE)).unwrap();

        assert_eq!(run.locate(claim_tail), Err(RunError::OutOfRange));
    }

    #[test]
    fn reusable_run_rejects_foreign_run_same_offset() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let base_a = runs.acquire(class, owner(), &pages).unwrap().range().base();
        let base_b = runs.acquire(class, owner(), &pages).unwrap().range().base();
        let (Some(PageOwner::Run(run_a)), Some(PageOwner::Run(run_b))) =
            (pages.get(base_a), pages.get(base_b))
        else {
            panic!("expected two published runs");
        };
        let ptr = alloc_block(run_b).unwrap();

        assert!(run_b.locate(ptr).is_ok());
        assert_eq!(run_a.locate(ptr), Err(RunError::OutOfRange));
    }

    #[cfg(feature = "safe")]
    #[test]
    fn claim_run_reports_duplicate_remote_free() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let ptr = alloc_block(run).unwrap();

        assert_eq!(run.claim(ptr), Ok(()));
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
    }

    #[test]
    fn accept_without_any_claim_is_a_noop() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.accept(), Accept::Done);
        // `ptr`'s block is still live (never claimed), so the next allocate is fresh.
        assert_ne!(alloc_block(run).unwrap(), ptr);
    }

    #[test]
    fn claim_accept_works_for_all_size_classes() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        for &size in &SizeClasses::SIZES {
            #[cfg(feature = "hardened")]
            let request = size - core::mem::size_of::<usize>();
            #[cfg(not(feature = "hardened"))]
            let request = size;
            let run = runs.acquire(class_id(request, 8), owner(), &pages).unwrap();
            let ptr = alloc_block(run).unwrap();
            assert_eq!(run.claim(ptr), Ok(()), "size={size}");
            run.push(ptr, ptr);
            assert_eq!(run.accept(), Accept::Done, "size={size}");
            let again = run.allocate();
            #[cfg(not(feature = "hardened"))]
            assert_eq!(again, Some(ptr), "size={size}");
            #[cfg(feature = "hardened")]
            assert_ne!(again, Some(ptr), "size={size}");
        }
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(17, 16);
        let run = runs.acquire(class, owner(), &pages).unwrap();
        let capacity = RUN_SIZE / class.size();

        for _ in 0..capacity {
            let ptr = alloc_block(run).unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn queue_wins_once_until_accept() {
        use super::super::inbox::Inbox;

        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let a = alloc_block(run).unwrap();
        let b = alloc_block(run).unwrap();
        let inbox: Inbox<'_, Run> = Inbox::new();

        assert_eq!(run.claim(a), Ok(()));
        run.push(a, a);
        assert!(inbox.enqueue(run));

        assert_eq!(run.claim(b), Ok(()));
        run.push(b, b);
        assert!(!inbox.enqueue(run));

        assert_eq!(inbox.drain().count(), 1);
        assert_eq!(run.accept(), Accept::Done);
        #[cfg(not(feature = "hardened"))]
        {
            assert_eq!(run.allocate(), Some(b));
            assert_eq!(run.allocate(), Some(a));
            assert_eq!(run.claim(a), Ok(()));
        }
        #[cfg(feature = "hardened")]
        {
            let fresh = alloc_block(run).unwrap();
            assert_eq!(run.claim(fresh), Ok(()));
        }
        assert!(inbox.enqueue(run));
    }

    /// A freer claims, pushes, and enqueues while the owner drains and requeues
    /// when `accept` returns [`Accept::Requeue`]. No pushed block stays off the freelist.
    #[test]
    fn accept_wakeup_proof_no_claim_is_ever_stranded() {
        use core::sync::atomic::AtomicBool;

        use super::super::inbox::Inbox;

        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, owner(), &pages).unwrap();
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
                    run.push(ptr, ptr);
                    inbox.enqueue(run);
                }
                done.store(true, Ordering::Release);
            });

            let mut spins = 0u32;
            loop {
                let finished = done.load(Ordering::Acquire);
                while !inbox.is_empty() {
                    for run in inbox.drain() {
                        if run.accept() == Accept::Requeue {
                            inbox.enqueue(run);
                        }
                    }
                }
                if finished
                    && inbox.is_empty()
                    && run.remote.chain.load(Ordering::Acquire).is_null()
                {
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
        let mut runs = RunHeap::new(
            RunConfig::new().with_policy(RunPolicy::Discard),
            Hints::new(),
        );
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
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
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        assert!(!run.is_discardable());
        let again = run.allocate();
        #[cfg(not(feature = "hardened"))]
        assert_eq!(again, Some(ptr));
        #[cfg(feature = "hardened")]
        assert_ne!(again, Some(ptr));
    }

    #[test]
    fn discard_after_accept_resets() {
        let mut runs = RunHeap::new(
            RunConfig::new().with_policy(RunPolicy::Discard),
            Hints::new(),
        );
        let pages = PageMap::new();
        let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        assert_eq!(run.claim(ptr), Ok(()));
        run.push(ptr, ptr);
        assert_eq!(run.accept(), Accept::Done);
        assert!(!run.is_live());
        assert!(run.allocate().is_none());
        assert_eq!(run.claim(ptr), Err(RunError::DoubleFree));
        assert!(run.extend());
        assert!(run.allocate().is_some());
    }

    /// Discard returns the payload pages to the OS and hands out the same
    /// space again, so dirty bytes read back as zero and the space stays writable.
    #[test]
    fn discard_zeroes_payload_and_keeps_it_mapped() {
        let mut runs = RunHeap::new(
            RunConfig::new().with_policy(RunPolicy::Discard),
            Hints::new(),
        );
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, owner(), &pages).unwrap();
        let ptr = alloc_block(run).unwrap();
        // SAFETY: `ptr` is a live block of `class.size()` bytes.
        unsafe { ptr.as_ptr().write_bytes(0x11, run.usable()) };
        assert_eq!(run.free(ptr), Ok(RunFree::Unchanged));
        run.discard();

        // SAFETY: the payload stays mapped for the process lifetime.
        let payload = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), run.usable()) };
        assert!(payload.iter().all(|&byte| byte == 0));
        assert!(run.extend());
        assert_eq!(run.allocate(), Some(ptr));
        // SAFETY: the block was just handed out again.
        unsafe {
            ptr.as_ptr().write(0x22);
            assert_eq!(ptr.as_ptr().read(), 0x22);
        }
    }

    #[cfg(feature = "hardened")]
    fn child_aborts(test: &str, var: &str, body: fn()) {
        use std::os::unix::process::ExitStatusExt;

        if std::env::var_os(var).is_some() {
            body();
            std::process::exit(0);
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1"])
            .env(var, "1")
            .status()
            .unwrap();
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "{test} exited with {status}"
        );
    }

    #[cfg(feature = "hardened")]
    #[test]
    fn delay_reuses_only_after_the_budget() {
        let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
        let pages = PageMap::new();
        let class = class_id(64, 8);
        let run = runs.acquire(class, owner(), &pages).unwrap();
        let first = alloc_block(run).unwrap();
        assert_eq!(run.free(first), Ok(RunFree::Unchanged));
        assert_ne!(alloc_block(run).unwrap(), first);

        // Leave one slot unissued so a full run does not push every free
        // straight back onto its freelist. Five runs clear the 256 KiB budget.
        for _ in 0..5 {
            let next = runs.acquire(class, owner(), &pages).unwrap();
            let capacity = RUN_SIZE / class.size();
            for _ in 0..capacity.saturating_sub(1) {
                let ptr = alloc_block(next).unwrap();
                next.free(ptr).unwrap();
            }
        }

        let mut found = false;
        for _ in 0..capacity_of(class) {
            let Some(ptr) = run.allocate() else { break };
            if ptr == first {
                found = true;
                break;
            }
        }
        assert!(found, "oldest block stayed delayed past the budget");
    }

    #[cfg(feature = "hardened")]
    fn capacity_of(class: SizeClass) -> usize {
        RUN_SIZE / class.size()
    }

    #[cfg(feature = "hardened")]
    #[test]
    fn damaged_run_checksum_aborts_on_free() {
        child_aborts(
            "heap::run::tests::damaged_run_checksum_aborts_on_free",
            "RUNIC_DAMAGE_RUN_FREE",
            || {
                let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
                let pages = PageMap::new();
                let run = runs.acquire(class_id(64, 8), owner(), &pages).unwrap();
                let ptr = alloc_block(run).unwrap();
                // SAFETY: the child corrupts the header so free aborts.
                unsafe {
                    core::ptr::addr_of!(run.checksum)
                        .cast_mut()
                        .write(Checksum::damaged());
                }
                let _ = run.free(ptr);
            },
        );
    }

    #[cfg(feature = "hardened")]
    #[test]
    fn damaged_run_checksum_aborts_on_acquire() {
        child_aborts(
            "heap::run::tests::damaged_run_checksum_aborts_on_acquire",
            "RUNIC_DAMAGE_RUN_ACQUIRE",
            || {
                let mut runs = RunHeap::new(RunConfig::new(), Hints::new());
                let pages = PageMap::new();
                let class = class_id(64, 8);
                let run = runs.acquire(class, owner(), &pages).unwrap();
                // SAFETY: the child corrupts the header so the next acquire aborts.
                unsafe {
                    core::ptr::addr_of!(run.checksum)
                        .cast_mut()
                        .write(Checksum::damaged());
                }
                runs.push_available(run).unwrap();
                let _ = runs.take_available(class);
            },
        );
    }
}
