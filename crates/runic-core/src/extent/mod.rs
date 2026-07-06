use core::{num::NonZeroU32, ptr::NonNull};

mod allocator;
mod mapping_cache;
mod table;

use crate::{
    layout::LayoutSpec,
    memory::{AddressRange, Mapping},
};

pub(crate) use allocator::{ExtentAllocator, ExtentAllocatorError};
pub(crate) use table::{ExtentReservation, ExtentTable};

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
}

pub(crate) struct Extent {
    id: ExtentId,
    mapping: Mapping,
    range: AddressRange,
}

impl Extent {
    pub(crate) fn new(id: ExtentId, mapping: Mapping, spec: LayoutSpec) -> Option<Self> {
        let user_addr = spec.align_addr(mapping.base().as_ptr().addr())?;
        let user_ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(user_addr))?;
        let range = AddressRange::new(user_ptr, spec.size());

        if mapping.range().contains(range) {
            Some(Self { id, mapping, range })
        } else {
            None
        }
    }

    pub(crate) const fn id(&self) -> ExtentId {
        self.id
    }

    pub(crate) const fn ptr(&self) -> NonNull<u8> {
        self.range.base()
    }

    pub(crate) fn starts_at(&self, ptr: NonNull<u8>) -> bool {
        ptr == self.ptr()
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn mapping_len(&self) -> usize {
        self.mapping.range().len()
    }

    pub(crate) fn into_mapping(self) -> Mapping {
        self.mapping
    }

    pub(crate) fn free(&self, ptr: NonNull<u8>) -> Result<(), ExtentError> {
        if self.starts_at(ptr) {
            Ok(())
        } else {
            Err(ExtentError::InvalidPointer)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{layout::LayoutSpec, memory::OsMemory};

    use super::*;

    #[test]
    fn extent_aligns_user_pointer_inside_mapping() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mapping_range = mapping.range();
        let extent = Extent::new(ExtentId::from_index(0).unwrap(), mapping, spec).unwrap();

        assert_eq!(extent.ptr().as_ptr() as usize % spec.align(), 0);
        assert_eq!(extent.range().len(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(1).unwrap(), mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::from_index(2).unwrap(), mapping, spec).unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }
}
