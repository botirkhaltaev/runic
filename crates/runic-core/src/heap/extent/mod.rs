use core::{
    cell::{Cell, UnsafeCell},
    num::NonZeroU32,
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
pub(crate) struct ExtentId {
    index: NonZeroU32,
}

impl ExtentId {
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
pub(crate) enum ExtentError {
    InvalidPointer,
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
    id: ExtentId,
    heap: &'static Heap,
    mapping: UnsafeCell<Option<Mapping>>,
    /// User base. Remote `claim` / holder `resize_in_place` share this word.
    base: AtomicPtr<u8>,
    /// User length. Holder-exclusive (`resize_in_place` / `reuse`).
    len: Cell<usize>,
    state: AtomicU8,
    /// Coalesced inbox membership (see `heap::inbox`). Only ever queued while
    /// exactly one claim can be outstanding (`Claimed`), so no bulk scan is needed —
    /// unlike `Run`, `accept` is a single exact-pointer transition.
    link: Link<Extent>,
    /// Rest of the cache / unmapped list. Owner-exclusive; never set while inbox-linked.
    next: Cell<Option<ExtentId>>,
    /// Pages known zero-filled while cached (Discard insert succeeded). Owner-exclusive.
    clean: Cell<bool>,
}

impl PartialEq for Extent {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

impl Eq for Extent {}

// SAFETY: before publication, an extent moves only under exclusive `ExtentHeap`
// access. After publication, remote methods touch `state`, `link`, `base`
// (`AtomicPtr`), `id`, `heap`, and `mapping` (immutable while published).
// `len`, `next`, and `clean` are written only under `HeapInner` or by the
// single allocation holder (`resize_in_place`). The explicit impls also break
// the recursive `Extent -> Heap -> ExtentHeap` auto-trait cycle.
unsafe impl Send for Extent {}
// SAFETY: shared remote access is `state` / `link` / `base` and immutable
// identity/mapping fields. `len` / `next` / `clean` are not read remotely.
unsafe impl Sync for Extent {}

impl Node for Extent {
    fn link(&self) -> &Link<Self> {
        &self.link
    }
}

impl Extent {
    pub(crate) fn new(
        id: ExtentId,
        heap: &'static Heap,
        mapping: Mapping,
        spec: LayoutSpec,
    ) -> Option<Self> {
        let user_addr = spec.align_addr(mapping.base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size().max(1));

        if mapping.range().contains(range) {
            Some(Self {
                id,
                heap,
                mapping: UnsafeCell::new(Some(mapping)),
                base: AtomicPtr::new(user_ptr.as_ptr()),
                len: Cell::new(range.len()),
                state: AtomicU8::new(ExtentState::Allocated.raw()),
                link: Link::new(),
                next: Cell::new(None),
                clean: Cell::new(false),
            })
        } else {
            None
        }
    }

    pub(crate) const fn id(&self) -> ExtentId {
        self.id
    }

    pub(crate) const fn heap(&self) -> &'static Heap {
        self.heap
    }

    pub(crate) fn next(&self) -> Option<ExtentId> {
        self.next.get()
    }

    pub(crate) fn set_next(&self, next: Option<ExtentId>) {
        self.next.set(next);
    }

    /// `MADV_DONTNEED` the mapping. Owner-exclusive; records whether advise succeeded.
    #[cold]
    pub(crate) fn discard(&self) {
        self.clean.set(Os::discard(self.mapping().range()));
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
        if !self.mapping().range().contains(requested) {
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
        self.state.store(ExtentState::Free.raw(), Ordering::Relaxed);
        self.base.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.len.set(0);
        self.clean.set(false);
        self.take_mapping()
    }

    pub(super) fn remount(&self, mapping: Mapping, spec: LayoutSpec) -> Option<NonNull<u8>> {
        let user_addr = spec.align_addr(mapping.base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size().max(1));
        if !mapping.range().contains(range) {
            return None;
        }
        // SAFETY: unmapped slot is owner-exclusive; previous mapping was taken.
        unsafe {
            *self.mapping.get() = Some(mapping);
        }
        self.base.store(user_ptr.as_ptr(), Ordering::Relaxed);
        self.len.set(range.len());
        self.clean.set(false);
        self.state
            .store(ExtentState::Allocated.raw(), Ordering::Relaxed);
        Some(user_ptr)
    }

    /// Owner-local free: exact pointer, then `Allocated → Free`. Owner DF is
    /// undefined. Remote admission is `claim` / `accept`.
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.validate_exact(ptr)?;
        self.state.store(ExtentState::Free.raw(), Ordering::Relaxed);
        Ok(())
    }

    /// Freer: exact pointer, then `Allocated → Claimed`.
    ///
    /// The state CAS may stay Relaxed: a successful claim is published by the
    /// inbox head's Release CAS, and owner accept follows an Acquire drain.
    pub(crate) fn claim(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.validate_exact(ptr)?;
        match self.state.compare_exchange(
            ExtentState::Allocated.raw(),
            ExtentState::Claimed.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(value) if value == ExtentState::Claimed.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    /// Owner: exact pointer `Claimed → Free`, then clear inbox queued.
    pub(crate) fn accept(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.validate_exact(ptr)?;
        match self.state.compare_exchange(
            ExtentState::Claimed.raw(),
            ExtentState::Free.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                self.link.clear_queued();
                Ok(())
            }
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    /// Owner-local reuse of a cached Free mapping. `init` zeros dirty Keep
    /// reuse: Discard-insert is already clean; Keep discards when `size ≥ 64 KiB`
    /// or memsets. Allocate-time Keep discard does not set [`Self::clean`].
    pub(crate) fn reuse(&self, spec: LayoutSpec, init: ExtentInit) -> Option<NonNull<u8>> {
        if self.load_state().ok()? != ExtentState::Free {
            return None;
        }

        let user_addr = spec.align_addr(self.mapping().base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size().max(1));
        if !self.mapping().range().contains(range) {
            return None;
        }

        let clean = self.clean.get();
        self.base.store(user_ptr.as_ptr(), Ordering::Relaxed);
        self.len.set(range.len());
        self.clean.set(false);
        self.state
            .store(ExtentState::Allocated.raw(), Ordering::Relaxed);
        if init == ExtentInit::Zeroed && !clean {
            self.zero_user(spec);
        }
        Some(self.ptr())
    }

    fn zero_user(&self, spec: LayoutSpec) {
        let zeroed = spec.size() >= LAZY_ZERO && Os::discard(self.mapping().range());
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

    static OWNER: Heap = Heap::new(
        HeapId::new(0, NonZeroU32::MIN).unwrap(),
        AllocatorConfig::new(),
    );

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    #[test]
    fn extent_equality_is_identity() {
        let spec = layout_spec(128 * 1024, 4096);
        let first = Extent::new(
            ExtentId::from_index(0).unwrap(),
            &OWNER,
            Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap(),
            spec,
        )
        .unwrap();
        let second = Extent::new(
            ExtentId::from_index(1).unwrap(),
            &OWNER,
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
        let extent = Extent::new(ExtentId::from_index(0).unwrap(), &OWNER, mapping, spec).unwrap();

        assert!(spec.is_addr_aligned(extent.ptr().as_ptr().addr()));
        assert_eq!(extent.len.get(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(1).unwrap(), &OWNER, mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(2).unwrap(), &OWNER, mapping, spec).unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_rejects_interior_claim_without_state_change() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(8).unwrap(), &OWNER, mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert_eq!(extent.claim(interior), Err(ExtentError::InvalidPointer));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_resizes_in_place_for_smaller_layout() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(3).unwrap(), &OWNER, mapping, spec).unwrap();
        let smaller = layout_spec(64 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), smaller), Ok(true));
    }

    #[test]
    fn extent_does_not_resize_in_place_beyond_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(4).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(false));
    }

    #[test]
    fn extent_grows_in_place_within_larger_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = Os::map(512 * 1024).unwrap();
        let extent = Extent::new(ExtentId::from_index(5).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.len.get(), 256 * 1024);
    }

    #[test]
    fn extent_grows_in_place_when_page_range_does_not_change() {
        let spec = layout_spec(33 * 1024, 8);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(6).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(36 * 1024, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.len.get(), 36 * 1024);
    }

    #[test]
    fn extent_does_not_resize_in_place_to_size_class() {
        let spec = layout_spec(64 * 1024, 8);
        let mapping = Os::map(spec.mapping_len(Os::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(7).unwrap(), &OWNER, mapping, spec).unwrap();
        let small = layout_spec(4096, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), small), Ok(false));
        assert_eq!(extent.len.get(), 64 * 1024);
    }
}
