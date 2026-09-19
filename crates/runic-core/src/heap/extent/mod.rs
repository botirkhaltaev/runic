use core::{
    cell::Cell,
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

mod cache;
pub(crate) mod config;
pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping, OsMemory},
    size_class::SizeClasses,
};

use super::{
    Heap, HeapId,
    inbox::{InboxLink, InboxNode},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentId {
    index: NonZeroU32,
}

impl ExtentId {
    pub(crate) fn from_index(index: u32) -> Option<Self> {
        NonZeroU32::new(index.checked_add(1)?).map(|index| Self { index })
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
    mapping: Mapping,
    range: Cell<AddressRange>,
    state: AtomicU8,
    /// Coalesced inbox membership (see `heap::inbox`). Only ever queued while
    /// exactly one claim can be outstanding (`Claimed`), so no bulk scan is needed —
    /// unlike `Run`, `accept` is a single exact-pointer transition.
    link: InboxLink<Extent>,
    /// Rest of the cache list. Owner-exclusive; never set while inbox-linked.
    next: Cell<Option<ExtentId>>,
    /// Mapping pages were `MADV_DONTNEED`'d after the last free. Owner-exclusive.
    discarded: Cell<bool>,
}

// SAFETY: before publication, an extent moves only under exclusive `ExtentHeap`
// access. After publication, remote operations touch only `state` and immutable
// identity/mapping fields; `range`, `next`, and `discarded` remain owner-exclusive.
// The explicit impls also break the recursive `Extent -> Heap -> ExtentHeap`
// auto-trait cycle without widening the unsafe boundary to the whole heap.
unsafe impl Send for Extent {}
// SAFETY: shared remote access is limited to the atomic state protocol and
// immutable fields; owner-only methods are serialized by `HeapInner`.
unsafe impl Sync for Extent {}

impl InboxNode for Extent {
    fn link(&self) -> &InboxLink<Self> {
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
                mapping,
                range: Cell::new(range),
                state: AtomicU8::new(ExtentState::Allocated.raw()),
                link: InboxLink::new(),
                next: Cell::new(None),
                discarded: Cell::new(false),
            })
        } else {
            None
        }
    }

    pub(crate) const fn id(&self) -> ExtentId {
        self.id
    }

    pub(crate) fn heap_id(&self) -> HeapId {
        self.heap.id()
    }

    pub(crate) fn next(&self) -> Option<ExtentId> {
        self.next.get()
    }

    pub(crate) fn set_next(&self, next: Option<ExtentId>) {
        self.next.set(next);
    }

    pub(crate) fn discarded(&self) -> bool {
        self.discarded.get()
    }

    /// `MADV_DONTNEED` the mapping. Owner-exclusive; records whether advise succeeded.
    #[cold]
    pub(crate) fn discard(&self) {
        self.discarded.set(OsMemory::discard(self.mapping.range()));
    }

    pub(crate) const fn ptr(&self) -> NonNull<u8> {
        self.range.get().base()
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
        if !self.mapping.range().contains(requested) {
            return Ok(false);
        }

        self.range.set(requested);

        Ok(true)
    }

    pub(crate) fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    /// Owner-local free: exact pointer, then `Allocated → Free`.
    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        self.validate_exact(ptr)?;
        match self.state.compare_exchange(
            ExtentState::Allocated.raw(),
            ExtentState::Free.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(value) if value == ExtentState::Claimed.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    /// Freer: exact pointer, then `Allocated → Claimed`.
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
            Err(value) if value == ExtentState::Claimed.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    /// Reuse a Free cached extent for `spec` without republishing its mapping.
    pub(crate) fn reuse(&self, spec: LayoutSpec) -> Option<NonNull<u8>> {
        if self.load_state().ok()? != ExtentState::Free {
            return None;
        }

        let user_addr = spec.align_addr(self.mapping.base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size().max(1));
        if !self.mapping.range().contains(range) {
            return None;
        }

        self.range.set(range);
        self.discarded.set(false);
        self.state
            .store(ExtentState::Allocated.raw(), Ordering::Relaxed);
        Some(self.ptr())
    }

    fn validate_exact(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        if self.starts_at(ptr) {
            Ok(())
        } else {
            Err(ExtentError::InvalidPointer)
        }
    }

    pub(crate) fn into_mapping(self) -> Mapping {
        self.mapping
    }

    fn load_state(&self) -> Result<ExtentState, ExtentError> {
        ExtentState::from_raw(self.state.load(Ordering::Relaxed)).ok_or(ExtentError::InvalidPointer)
    }
}

#[cfg(test)]
mod tests {
    use core::{alloc::Layout, num::NonZeroU32};
    use std::sync::LazyLock;

    use crate::{config::AllocatorConfig, heap::Heap, layout::LayoutSpec, memory::OsMemory};

    use super::*;

    static OWNER: LazyLock<Heap> = LazyLock::new(|| {
        Heap::new(
            HeapId::new(0, NonZeroU32::MIN).unwrap(),
            AllocatorConfig::new(),
        )
    });

    fn layout_spec(size: usize, align: usize) -> LayoutSpec {
        LayoutSpec::from_layout(Layout::from_size_align(size, align).unwrap())
    }

    #[test]
    fn extent_aligns_user_pointer_inside_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mapping_range = mapping.range();
        let extent = Extent::new(ExtentId::from_index(0).unwrap(), &OWNER, mapping, spec).unwrap();

        assert!(spec.is_addr_aligned(extent.ptr().as_ptr() as usize));
        assert_eq!(extent.range.get().len(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(1).unwrap(), &OWNER, mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(2).unwrap(), &OWNER, mapping, spec).unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_rejects_interior_claim_without_state_change() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(8).unwrap(), &OWNER, mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert_eq!(extent.claim(interior), Err(ExtentError::InvalidPointer));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_resizes_in_place_for_smaller_layout() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(3).unwrap(), &OWNER, mapping, spec).unwrap();
        let smaller = layout_spec(64 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), smaller), Ok(true));
    }

    #[test]
    fn extent_does_not_resize_in_place_beyond_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(4).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(false));
    }

    #[test]
    fn extent_grows_in_place_within_larger_mapping() {
        let spec = layout_spec(128 * 1024, 4096);
        let mapping = OsMemory::map(512 * 1024).unwrap();
        let extent = Extent::new(ExtentId::from_index(5).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(256 * 1024, 4096);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.range.get().len(), 256 * 1024);
    }

    #[test]
    fn extent_grows_in_place_when_page_range_does_not_change() {
        let spec = layout_spec(33 * 1024, 8);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(6).unwrap(), &OWNER, mapping, spec).unwrap();
        let larger = layout_spec(36 * 1024, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.range.get().len(), 36 * 1024);
    }

    #[test]
    fn extent_does_not_resize_in_place_to_size_class() {
        let spec = layout_spec(64 * 1024, 8);
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(7).unwrap(), &OWNER, mapping, spec).unwrap();
        let small = layout_spec(4096, 8);

        assert_eq!(extent.resize_in_place(extent.ptr(), small), Ok(false));
        assert_eq!(extent.range.get().len(), 64 * 1024);
    }
}
