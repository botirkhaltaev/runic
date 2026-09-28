use core::{
    cell::{Cell, UnsafeCell},
    ptr::{NonNull, write_bytes},
    sync::atomic::{AtomicPtr, AtomicU8, Ordering},
};

mod cache;
pub(crate) mod config;
pub(crate) mod heap;

use crate::{
    allocator::Allocator,
    layout::LayoutSpec,
    memory::{AddressRange, Mapping, Memory, Os},
    size_class::SizeClasses,
};

#[cfg(feature = "hardened")]
use super::checksum::Checksum;
use super::list::{self, Linked as ListLinked};
use super::{
    Heap,
    inbox::{Link, Node},
};

/// Zeroed Keep reuse at or above this size discards instead of memset.
pub(super) const LAZY_ZERO: usize = 64 * 1024;

/// How a newly allocated extent's bytes should be initialized.
///
/// Fresh anonymous mappings are already kernel-zeroed. Cached extents may be
/// dirty, so [`ExtentInit::Zeroed`] zeros on cache hits: Discard-insert already
/// dropped the pages, else Keep discards when `size ≥ 64 KiB` or memsets.
/// Allocate-time Keep discard does not set [`Extent::clean`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentInit {
    Uninit,
    Zeroed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentError {
    InvalidPointer,
    /// Second `claim` or `accept`. Fast leaves that free undefined.
    #[cfg(any(feature = "safe", feature = "hardened"))]
    DoubleFree,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExtentState {
    Free = 0,
    Allocated = 1,
    Claimed = 2,
}

impl ExtentState {
    const fn raw(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Allocated => 1,
            Self::Claimed => 2,
        }
    }

    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            value if value == Self::Free.raw() => Some(Self::Free),
            value if value == Self::Allocated.raw() => Some(Self::Allocated),
            value if value == Self::Claimed.raw() => Some(Self::Claimed),
            _ => None,
        }
    }
}

pub(crate) struct Extent {
    heap: &'static Heap,
    mapping: UnsafeCell<Option<Mapping>>,
    /// User base. Remote `claim` / holder `resize_in_place` share this word.
    base: AtomicPtr<u8>,
    /// User length. Holder-exclusive (`resize_in_place` / `reuse`).
    len: Cell<usize>,
    /// `Allocated` / `Claimed` / `Free`. Remote claim and the owner share it, so
    /// it stays atomic. Fast stores the next state. `safe` CASes it.
    state: AtomicU8,
    /// Pages known zero-filled while cached (Discard insert succeeded). Owner-exclusive.
    clean: Cell<bool>,
    /// Coalesced inbox membership (see `heap::inbox`). Only ever enqueued while
    /// exactly one claim can be outstanding (`Claimed`), so no bulk scan is needed —
    /// unlike `Run`, `accept` is a single exact-pointer transition.
    inbox: Link<Extent>,
    /// Cache, unmapped-slot, or accepted list. Owner-exclusive; a slot is on at most one.
    slot: list::Link<Extent>,
    /// Owner, mapping base, and mapping length. Checked on reuse and free.
    #[cfg(feature = "hardened")]
    checksum: Cell<Checksum>,
}

impl PartialEq for Extent {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

impl Eq for Extent {}

// SAFETY: before publication, an extent moves only under exclusive `ExtentHeap`
// access. After publication, remote methods touch `state`, `inbox`, `base`
// (`AtomicPtr`), `heap`, and `mapping` (immutable while published).
// `len`, `slot`, and `clean` are written only under `HeapInner` or by the
// single allocation holder (`resize_in_place`). The explicit impls also break
// the recursive `Extent -> Heap -> ExtentHeap` auto-trait cycle.
unsafe impl Send for Extent {}
// SAFETY: shared remote access is `state` / `inbox` / `base` and immutable
// identity/mapping fields. `len` / `slot` / `clean` are not read remotely.
unsafe impl Sync for Extent {}

impl Node for Extent {
    fn link(&self) -> &Link<Self> {
        &self.inbox
    }
}

impl ListLinked for Extent {
    fn links(&self) -> &list::Link<Self> {
        &self.slot
    }
}

impl Extent {
    pub(crate) fn new(heap: &'static Heap, mapping: Mapping, spec: LayoutSpec) -> Option<Self> {
        let range = mapping.place(spec)?;
        #[cfg(feature = "hardened")]
        let checksum = Checksum::of(Self::digest(heap, &mapping));
        Some(Self {
            heap,
            mapping: UnsafeCell::new(Some(mapping)),
            base: AtomicPtr::new(range.base().as_ptr()),
            len: Cell::new(range.len()),
            state: AtomicU8::new(ExtentState::Allocated.raw()),
            clean: Cell::new(false),
            inbox: Link::new(),
            slot: list::Link::new(),
            #[cfg(feature = "hardened")]
            checksum: Cell::new(checksum),
        })
    }

    /// User bytes. Guard pages stay outside this range.
    pub(crate) fn payload(&self) -> AddressRange {
        self.mapping().payload()
    }

    /// The slot words the checksum covers.
    #[cfg(feature = "hardened")]
    fn digest(heap: &Heap, mapping: &Mapping) -> [usize; 3] {
        [
            core::ptr::from_ref(heap).addr(),
            mapping.base().as_ptr().addr(),
            mapping.len().get(),
        ]
    }

    /// Abort when the slot no longer matches its mapping. No-op on Fast.
    #[inline]
    pub(crate) fn check_header(&self) {
        #[cfg(feature = "hardened")]
        if self.checksum.get() != Checksum::of(Self::digest(self.heap, self.mapping())) {
            Allocator::abort();
        }
        #[cfg(not(feature = "hardened"))]
        {
            let _ = self;
        }
    }

    /// Record `mapping` as this slot's current mapping.
    fn seal(&self, mapping: &Mapping) {
        #[cfg(feature = "hardened")]
        self.checksum
            .set(Checksum::of(Self::digest(self.heap, mapping)));
        #[cfg(not(feature = "hardened"))]
        {
            let _ = (self, mapping);
        }
    }

    pub(crate) const fn heap(&self) -> &'static Heap {
        self.heap
    }

    /// `MADV_DONTNEED` the mapping. Owner-exclusive; records whether advise succeeded.
    #[cold]
    pub(crate) fn discard(&self) {
        self.clean.set(Os::discard(self.payload()));
    }

    pub(crate) fn ptr(&self) -> NonNull<u8> {
        NonNull::new(self.base.load(Ordering::Relaxed)).unwrap_or_else(|| Allocator::abort())
    }

    /// Holder-side user length (`malloc_usable_size` / C realloc).
    pub(crate) fn len(&self) -> usize {
        self.len.get()
    }

    /// Allocated or claimed — cached Free extents are not live.
    pub(crate) fn is_live(&self) -> bool {
        matches!(
            self.load_state(),
            Ok(ExtentState::Allocated | ExtentState::Claimed)
        )
    }

    pub(crate) fn starts_at(&self, ptr: NonNull<u8>) -> bool {
        ptr == self.ptr()
    }

    pub(crate) fn resize_in_place(
        &self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, ExtentError> {
        if !self.starts_at(ptr) {
            return Err(ExtentError::InvalidPointer);
        }

        // Small dealloc uses `Run::header_of`. An in-place shrink to a size class
        // would leave a live extent behind a small layout and fault on the
        // unmapped header page. Force allocate-copy-free instead.
        if SizeClasses::class_for(spec).is_some() {
            return Ok(false);
        }

        if !spec.is_addr_aligned(ptr.as_ptr().addr()) {
            return Ok(false);
        }

        let requested = AddressRange::new(ptr, spec.size().max(1));
        if !self.payload().contains(requested) {
            return Ok(false);
        }

        self.base.store(ptr.as_ptr(), Ordering::Relaxed);
        self.len.set(requested.len());

        Ok(true)
    }

    pub(crate) fn mapping(&self) -> &Mapping {
        // SAFETY: published extents hold `Some` until owner `take_mapping`.
        unsafe { (*self.mapping.get()).as_ref() }.unwrap_or_else(|| Allocator::abort())
    }

    pub(super) fn take_mapping(&self) -> Option<Mapping> {
        // SAFETY: owner-exclusive unmap; no concurrent `mapping()` after this.
        unsafe { (*self.mapping.get()).take() }
    }

    pub(super) fn unmount(&self) -> Option<Mapping> {
        self.clear_user();
        self.take_mapping()
    }

    pub(super) fn remount(&self, mapping: Mapping, spec: LayoutSpec) -> Option<NonNull<u8>> {
        let range = mapping.place(spec)?;
        self.seal(&mapping);
        // SAFETY: unmapped slot is owner-exclusive; previous mapping was taken.
        unsafe {
            *self.mapping.get() = Some(mapping);
        }
        Some(self.mount(range))
    }

    /// Publish the user range of a mapping this extent already holds.
    fn mount(&self, range: AddressRange) -> NonNull<u8> {
        self.base.store(range.base().as_ptr(), Ordering::Relaxed);
        self.len.set(range.len());
        self.clean.set(false);
        self.state
            .store(ExtentState::Allocated.raw(), Ordering::Relaxed);
        range.base()
    }

    fn clear_user(&self) {
        self.state.store(ExtentState::Free.raw(), Ordering::Relaxed);
        self.base.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.len.set(0);
        self.clean.set(false);
    }

    /// Owner-local free: exact pointer, then `Allocated → Free`.
    ///
    /// A second owner free is undefined on Fast. `safe` reports `DoubleFree`,
    /// which the allocator aborts on, so the extent never enters the cache
    /// twice. Remote admission is `claim` / `accept`.
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.check_header();
        self.transition(ptr, ExtentState::Allocated, ExtentState::Free)
    }

    /// Freer: exact pointer, then `Allocated → Claimed`.
    ///
    /// A second claim is undefined on Fast and `DoubleFree` on `safe`. The
    /// inbox link coalesces a second enqueue either way.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.transition(ptr, ExtentState::Allocated, ExtentState::Claimed)
    }

    /// Owner: exact pointer, `Claimed → Free`, then store the inbox link idle.
    pub(crate) fn accept(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.check_header();
        self.transition(ptr, ExtentState::Claimed, ExtentState::Free)?;
        self.inbox.idle();
        Ok(())
    }

    /// Exact pointer, then the state byte `from → to`.
    ///
    /// Fast stores `to`; a byte that is not `from` is a double free and stays
    /// undefined. `safe` compares first: a byte already past `from` is
    /// `DoubleFree`, anything else `InvalidPointer`. Relaxed suffices because a
    /// claim is published by the inbox head's Release CAS and the owner's
    /// accept follows an Acquire drain.
    fn transition(
        &self,
        ptr: NonNull<u8>,
        from: ExtentState,
        to: ExtentState,
    ) -> Result<(), ExtentError> {
        self.validate_exact(ptr)?;
        #[cfg(any(feature = "safe", feature = "hardened"))]
        {
            self.state
                .compare_exchange(from.raw(), to.raw(), Ordering::Relaxed, Ordering::Relaxed)
                .map(|_| ())
                .map_err(|found| match ExtentState::from_raw(found) {
                    Some(ExtentState::Free | ExtentState::Claimed) => ExtentError::DoubleFree,
                    Some(ExtentState::Allocated) | None => ExtentError::InvalidPointer,
                })
        }
        #[cfg(not(any(feature = "safe", feature = "hardened")))]
        {
            debug_assert_eq!(self.state.load(Ordering::Relaxed), from.raw());
            self.state.store(to.raw(), Ordering::Relaxed);
            Ok(())
        }
    }

    /// Owner-local reuse of a cached Free mapping. `init` zeros dirty Keep
    /// reuse: Discard-insert is already clean; Keep discards when `size ≥ 64 KiB`
    /// or memsets. Allocate-time Keep discard does not set [`Self::clean`].
    pub(crate) fn reuse(&self, spec: LayoutSpec, init: ExtentInit) -> Option<NonNull<u8>> {
        self.check_header();
        if self.load_state().ok()? != ExtentState::Free {
            return None;
        }
        let range = self.mapping().place(spec)?;
        let clean = self.clean.get();
        let ptr = self.mount(range);
        if init == ExtentInit::Zeroed && !clean {
            self.zero_user(spec);
        }
        Some(ptr)
    }

    fn zero_user(&self, spec: LayoutSpec) {
        let zeroed = spec.size() >= LAZY_ZERO && Os::discard(self.payload());
        if !zeroed {
            // SAFETY: `reuse` just allocated this user range for `spec`.
            unsafe { write_bytes(self.ptr().as_ptr(), 0, spec.size()) };
        }
    }

    fn validate_exact(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        if self.starts_at(ptr) {
            Ok(())
        } else {
            Err(ExtentError::InvalidPointer)
        }
    }

    fn load_state(&self) -> Result<ExtentState, ExtentError> {
        ExtentState::from_raw(self.state.load(Ordering::Relaxed)).ok_or(ExtentError::InvalidPointer)
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, num::NonZeroU32};

    use crate::{
        config::AllocatorConfig,
        heap::{Heap, HeapId},
        layout::LayoutSpec,
    };

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

    #[test]
    fn extent_equality_is_identity() {
        let spec = layout_spec(128 * 1024, 4096);
        let first = Extent::new(
            owner(),
            Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap(),
            spec,
        )
        .unwrap();
        let second = Extent::new(
            owner(),
            Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap(),
            spec,
        )
        .unwrap();

        assert!(first == first);
        assert!(first != second);
    }

    #[test]
    fn extent_aligns_user_pointer_inside_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let mapping_range = mapping.range();
        let extent = Extent::new(owner(), mapping, spec).unwrap();

        assert!(spec.is_addr_aligned(extent.ptr().as_ptr().addr()));
        assert_eq!(extent.len.get(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let interior = NonNull::new(extent.ptr().as_ptr().wrapping_add(1)).unwrap();

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_rejects_interior_claim_without_state_change() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let interior = NonNull::new(extent.ptr().as_ptr().wrapping_add(1)).unwrap();

        assert_eq!(extent.claim(interior), Err(ExtentError::InvalidPointer));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[cfg(any(feature = "safe", feature = "hardened"))]
    #[test]
    fn extent_rejects_second_claim() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let ptr = extent.ptr();

        assert_eq!(extent.claim(ptr), Ok(()));
        assert_eq!(extent.claim(ptr), Err(ExtentError::DoubleFree));
        assert_eq!(extent.accept(ptr), Ok(()));
        assert_eq!(extent.accept(ptr), Err(ExtentError::DoubleFree));
    }

    #[test]
    fn extent_resizes_in_place_for_smaller_layout() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let smaller = layout_spec(64 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), smaller), Ok(true));
    }

    #[test]
    fn extent_does_not_resize_in_place_beyond_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(false));
    }

    #[test]
    fn extent_grows_in_place_within_larger_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(512 * 1024).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.len.get(), 256 * 1024);
    }

    #[test]
    fn extent_grows_in_place_when_page_range_does_not_change() {
        let spec = layout_spec(33 * 1024, 8);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let larger = layout_spec(36 * 1024, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.len.get(), 36 * 1024);
    }

    #[test]
    fn extent_does_not_resize_in_place_to_size_class() {
        let spec = layout_spec(64 * 1024, 8);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(owner(), mapping, spec).unwrap();
        let small = layout_spec(4096, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), small), Ok(false));
        assert_eq!(extent.len.get(), 64 * 1024);
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
    fn damaged_extent_checksum_aborts_on_free() {
        child_aborts(
            "heap::extent::tests::damaged_extent_checksum_aborts_on_free",
            "RUNIC_DAMAGE_EXTENT_FREE",
            || {
                let spec = layout_spec(128 * 1024, 4096);
                let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
                let extent = Extent::new(owner(), mapping, spec).unwrap();
                extent.checksum.set(Checksum::damaged());
                let _ = extent.free(extent.ptr());
            },
        );
    }

    #[cfg(feature = "hardened")]
    #[test]
    fn damaged_extent_checksum_aborts_on_reuse() {
        child_aborts(
            "heap::extent::tests::damaged_extent_checksum_aborts_on_reuse",
            "RUNIC_DAMAGE_EXTENT_REUSE",
            || {
                let spec = layout_spec(128 * 1024, 4096);
                let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
                let extent = Extent::new(owner(), mapping, spec).unwrap();
                extent.free(extent.ptr()).unwrap();
                extent.checksum.set(Checksum::damaged());
                let _ = extent.reuse(spec, ExtentInit::Uninit);
            },
        );
    }
}
