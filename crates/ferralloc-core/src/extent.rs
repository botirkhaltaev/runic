use core::ptr::NonNull;

use crate::{address::AddressRange, layout::LayoutSpec, os_memory::Mapping};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentId(u32);

impl ExtentId {
    pub(crate) const INVALID_RAW: u32 = u32::MAX;

    pub(crate) const fn new(raw: u32) -> Option<Self> {
        if raw == Self::INVALID_RAW {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
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
    use crate::{layout::LayoutSpec, os_memory::OsMemory};

    use super::*;

    #[test]
    fn extent_aligns_user_pointer_inside_mapping() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let mapping_range = mapping.range();
        let extent = Extent::new(ExtentId::new(0).unwrap(), mapping, spec).unwrap();

        assert_eq!(extent.ptr().as_ptr() as usize % spec.align(), 0);
        assert_eq!(extent.range().len(), spec.size());
        assert!(mapping_range.offset_of(extent.ptr()).is_some());
    }

    #[test]
    fn extent_rejects_interior_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::new(1).unwrap(), mapping, spec).unwrap();
        // SAFETY: adding one stays within the mapped extent for this non-zero allocation.
        let interior = unsafe { NonNull::new_unchecked(extent.ptr().as_ptr().add(1)) };

        assert!(!extent.starts_at(interior));
        assert_eq!(extent.free(interior), Err(ExtentError::InvalidPointer));
    }

    #[test]
    fn extent_accepts_exact_pointer() {
        let spec = LayoutSpec::from_size_align(128 * 1024, 4096).unwrap();
        let mapping = OsMemory::map(spec.mapping_len(OsMemory::page_size()).unwrap()).unwrap();
        let extent = Extent::new(ExtentId::new(2).unwrap(), mapping, spec).unwrap();

        assert!(extent.starts_at(extent.ptr()));
        assert_eq!(extent.free(extent.ptr()), Ok(()));
    }
}
