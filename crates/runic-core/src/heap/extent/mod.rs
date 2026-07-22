use core::{
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

mod cache;
pub(crate) mod heap;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
};

use super::HeapId;

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
    RemotePending = 2,
}

impl ExtentState {
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

pub(crate) struct Extent {
    id: ExtentId,
    heap: HeapId,
    mapping: Mapping,
    range: AddressRange,
    state: AtomicU8,
}

impl Extent {
    pub(crate) fn new(
        id: ExtentId,
        heap: HeapId,
        mapping: Mapping,
        spec: LayoutSpec,
    ) -> Option<Self> {
        let user_addr = spec.align_addr(mapping.base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size());

        if mapping.range().contains(range) {
            Some(Self {
                id,
                heap,
                mapping,
                range,
                state: AtomicU8::new(ExtentState::Allocated.raw()),
            })
        } else {
            None
        }
    }

    pub(crate) const fn id(&self) -> ExtentId {
        self.id
    }

    pub(crate) const fn heap_id(&self) -> HeapId {
        self.heap
    }

    pub(crate) const fn ptr(&self) -> NonNull<u8> {
        self.range.base()
    }

    pub(crate) fn starts_at(&self, ptr: NonNull<u8>) -> bool {
        ptr == self.ptr()
    }

    pub(crate) fn resize_in_place(
        &mut self,
        ptr: NonNull<u8>,
        spec: LayoutSpec,
    ) -> Result<bool, ExtentError> {
        if !self.starts_at(ptr) {
            return Err(ExtentError::InvalidPointer);
        }

        if !spec.is_addr_aligned(ptr.as_ptr().addr()) {
            return Ok(false);
        }

        let requested = AddressRange::new(ptr, spec.size());
        if !self.mapping.range().contains(requested) {
            return Ok(false);
        }

        self.range = requested;

        Ok(true)
    }

    pub(crate) fn mapping_range(&self) -> AddressRange {
        self.mapping.range()
    }

    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        match self.state.compare_exchange(
            ExtentState::Allocated.raw(),
            ExtentState::Free.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => self.validate_free(ptr),
            Err(value) if value == ExtentState::RemotePending.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    pub(crate) fn claim_free(&self) -> Result<(), ExtentError> {
        match self.state.compare_exchange(
            ExtentState::Allocated.raw(),
            ExtentState::RemotePending.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(value) if value == ExtentState::RemotePending.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    pub(crate) fn unclaim(&self) -> Result<(), ExtentError> {
        match self.state.compare_exchange(
            ExtentState::RemotePending.raw(),
            ExtentState::Allocated.raw(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(value) if value == ExtentState::RemotePending.raw() => Err(ExtentError::DoubleFree),
            Err(value) if value == ExtentState::Free.raw() => Err(ExtentError::DoubleFree),
            Err(_) => Err(ExtentError::InvalidPointer),
        }
    }

    pub(crate) fn validate_remote_pending(&self) -> Result<(), ExtentError> {
        if self.load_state()? == ExtentState::RemotePending {
            Ok(())
        } else {
            Err(ExtentError::DoubleFree)
        }
    }

    pub(crate) fn validate_free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
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
    use core::num::NonZeroU32;

    use crate::{layout::LayoutSpec, memory::OsMemory};

    use super::*;

    fn test_heap_id() -> HeapId {
        HeapId::new(0, NonZeroU32::MIN).unwrap()
    }

    #[test]
    fn extent_aligns_user_pointer_inside_mapping() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mapping_range = mapping.range();
        let extent = Extent::new(
            ExtentId::from_index(0).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();

        assert_eq!(extent.ptr().as_ptr() as usize % spec.align(), 0);
        assert_eq!(extent.range.len(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(
            ExtentId::from_index(1).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(
            ExtentId::from_index(2).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_unclaim_restores_allocated() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(
            ExtentId::from_index(7).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();

        assert_eq!(extent.claim_free(), Ok(()));
        assert_eq!(extent.unclaim(), Ok(()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }

    #[test]
    fn extent_resizes_in_place_for_smaller_layout() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mut extent = Extent::new(
            ExtentId::from_index(3).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();
        let smaller = LayoutSpec::from_size_align(64 * 1024, 4096).unwrap();

        assert_eq!(extent.resize_in_place(extent.ptr(), smaller), Ok(true));
    }

    #[test]
    fn extent_does_not_resize_in_place_beyond_mapping() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mut extent = Extent::new(
            ExtentId::from_index(4).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();
        let larger = LayoutSpec::from_size_align(256 * 1024, 4096).unwrap();

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(false));
    }

    #[test]
    fn extent_grows_in_place_within_larger_mapping() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(512 * 1024).unwrap();
        let mut extent = Extent::new(
            ExtentId::from_index(5).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();
        let larger = LayoutSpec::from_size_align(256 * 1024, 4096).unwrap();

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.range.len(), 256 * 1024);
    }

    #[test]
    fn extent_grows_in_place_when_page_range_does_not_change() {
        let spec = LayoutSpec::from_size_align(4095, 8).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mut extent = Extent::new(
            ExtentId::from_index(6).unwrap(),
            test_heap_id(),
            mapping,
            spec,
        )
        .unwrap();
        let larger = LayoutSpec::from_size_align(4096, 8).unwrap();

        assert_eq!(extent.resize_in_place(extent.ptr(), larger), Ok(true));
        assert_eq!(extent.range.len(), 4096);
    }
}
