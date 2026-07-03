use core::ptr::NonNull;

use crate::{
    os_memory::{OsMemory, PAGE_SIZE},
    span::SpanId,
};

const PAGE_SHIFT: usize = 12;
const L2_BITS: usize = 12;
const L2_ENTRIES: usize = 1 << L2_BITS;
const L1_ENTRIES: usize = 1 << (48 - PAGE_SHIFT - L2_BITS);

type SpanMapEntry = u32;

#[repr(C)]
struct L2Table {
    entries: [SpanMapEntry; L2_ENTRIES],
}

pub(crate) struct SpanMap {
    l1: Option<NonNull<*mut L2Table>>,
}

unsafe impl Send for SpanMap {}

impl SpanMap {
    pub(crate) const fn new() -> Self {
        Self { l1: None }
    }

    pub(crate) fn get(&self, ptr: NonNull<u8>) -> Option<SpanId> {
        let (l1_index, l2_index) = Self::indexes(ptr.as_ptr() as usize)?;
        let l1 = self.l1?;
        let l2 = unsafe { *l1.as_ptr().add(l1_index) };

        if l2.is_null() {
            return None;
        }

        let raw = unsafe { (*l2).entries[l2_index] };
        raw.checked_sub(1).and_then(SpanId::new)
    }

    pub(crate) fn insert_range(
        &mut self,
        base: NonNull<u8>,
        len: usize,
        id: SpanId,
        memory: &OsMemory,
    ) -> bool {
        let Some((first, last)) = Self::page_range(base, len) else {
            return false;
        };

        for page in first..last {
            let Some((l1_index, l2_index)) = Self::page_indexes(page) else {
                return false;
            };

            let Some(l2) = self.l2_for_insert(l1_index, memory) else {
                return false;
            };

            let existing = unsafe { (*l2.as_ptr()).entries[l2_index] };

            if existing != 0 && existing != id.get().saturating_add(1) {
                return false;
            }
        }

        for page in first..last {
            let Some((l1_index, l2_index)) = Self::page_indexes(page) else {
                return false;
            };
            let Some(l1) = self.l1 else {
                return false;
            };
            let l2 = unsafe { *l1.as_ptr().add(l1_index) };

            unsafe {
                (*l2).entries[l2_index] = id.get().saturating_add(1);
            }
        }

        true
    }

    pub(crate) fn remove_range(&mut self, base: NonNull<u8>, len: usize) {
        let Some((first, last)) = Self::page_range(base, len) else {
            return;
        };

        for page in first..last {
            let Some((l1_index, l2_index)) = Self::page_indexes(page) else {
                continue;
            };
            let Some(l1) = self.l1 else {
                return;
            };
            let l2 = unsafe { *l1.as_ptr().add(l1_index) };

            if l2.is_null() {
                continue;
            }

            unsafe {
                (*l2).entries[l2_index] = 0;
            }
        }
    }

    fn l2_for_insert(&mut self, l1_index: usize, memory: &OsMemory) -> Option<NonNull<L2Table>> {
        let l1 = self.l1_or_init(memory)?;
        let slot = unsafe { l1.as_ptr().add(l1_index) };
        let existing = unsafe { *slot };

        if let Some(existing) = NonNull::new(existing) {
            return Some(existing);
        }

        let mapping = memory.map(core::mem::size_of::<L2Table>())?;
        let table = mapping.base().cast::<L2Table>();
        unsafe {
            *slot = table.as_ptr();
        }

        Some(table)
    }

    fn l1_or_init(&mut self, memory: &OsMemory) -> Option<NonNull<*mut L2Table>> {
        if let Some(l1) = self.l1 {
            return Some(l1);
        }

        let len = L1_ENTRIES.checked_mul(core::mem::size_of::<*mut L2Table>())?;
        let mapping = memory.map(len)?;
        let l1 = mapping.base().cast::<*mut L2Table>();
        self.l1 = Some(l1);
        Some(l1)
    }

    fn page_range(base: NonNull<u8>, len: usize) -> Option<(usize, usize)> {
        let start = (base.as_ptr() as usize) >> PAGE_SHIFT;
        let end_addr = (base.as_ptr() as usize).checked_add(len.checked_sub(1)?)?;
        let end = (end_addr >> PAGE_SHIFT).checked_add(1)?;
        Some((start, end))
    }

    fn indexes(addr: usize) -> Option<(usize, usize)> {
        Self::page_indexes(addr >> PAGE_SHIFT)
    }

    fn page_indexes(page: usize) -> Option<(usize, usize)> {
        let l2 = page & (L2_ENTRIES - 1);
        let l1 = page >> L2_BITS;

        if l1 >= L1_ENTRIES {
            return None;
        }

        Some((l1, l2))
    }
}

const _: () = assert!(PAGE_SIZE == 1 << PAGE_SHIFT);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os_memory::OsMemory;

    fn id(raw: u32) -> SpanId {
        SpanId::new(raw).unwrap()
    }

    #[test]
    fn span_map_new_lookup_returns_none() {
        let map = SpanMap::new();
        let ptr = NonNull::dangling();

        assert!(map.get(ptr).is_none());
    }

    #[test]
    fn span_map_insert_range_maps_interior_pointer() {
        let memory = OsMemory::new();
        let mapping = memory.map(PAGE_SIZE * 2).unwrap();
        let mut map = SpanMap::new();

        assert!(map.insert_range(mapping.base(), mapping.len(), id(7), &memory));

        let interior =
            unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(PAGE_SIZE + 17)) };
        assert_eq!(map.get(interior), Some(id(7)));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_map_remove_range_clears_mapped_pages() {
        let memory = OsMemory::new();
        let mapping = memory.map(PAGE_SIZE * 2).unwrap();
        let mut map = SpanMap::new();

        assert!(map.insert_range(mapping.base(), mapping.len(), id(8), &memory));
        map.remove_range(mapping.base(), mapping.len());

        assert!(map.get(mapping.base()).is_none());
        let second = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(PAGE_SIZE)) };
        assert!(map.get(second).is_none());

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_map_remove_range_preserves_neighboring_page() {
        let memory = OsMemory::new();
        let mapping = memory.map(PAGE_SIZE * 3).unwrap();
        let mut map = SpanMap::new();
        let first = mapping.base();
        let second = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(PAGE_SIZE)) };
        let third = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(PAGE_SIZE * 2)) };

        assert!(map.insert_range(first, PAGE_SIZE, id(1), &memory));
        assert!(map.insert_range(second, PAGE_SIZE, id(2), &memory));
        assert!(map.insert_range(third, PAGE_SIZE, id(3), &memory));

        map.remove_range(second, PAGE_SIZE);

        assert_eq!(map.get(first), Some(id(1)));
        assert!(map.get(second).is_none());
        assert_eq!(map.get(third), Some(id(3)));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_map_insert_range_rejects_overlapping_different_span() {
        let memory = OsMemory::new();
        let mapping = memory.map(PAGE_SIZE * 2).unwrap();
        let mut map = SpanMap::new();
        let second = unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(PAGE_SIZE)) };

        assert!(map.insert_range(mapping.base(), PAGE_SIZE * 2, id(11), &memory));
        assert!(!map.insert_range(second, PAGE_SIZE, id(12), &memory));
        assert_eq!(map.get(second), Some(id(11)));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_map_insert_range_rejects_zero_len() {
        let memory = OsMemory::new();
        let mapping = memory.map(PAGE_SIZE).unwrap();
        let mut map = SpanMap::new();

        assert!(!map.insert_range(mapping.base(), 0, id(9), &memory));

        unsafe { memory.unmap(mapping) };
    }

    #[test]
    fn span_map_insert_range_crosses_l2_boundary() {
        let memory = OsMemory::new();
        let len = (L2_ENTRIES + 2) * PAGE_SIZE;
        let mapping = memory.map(len).unwrap();
        let mut map = SpanMap::new();

        assert!(map.insert_range(mapping.base(), mapping.len(), id(10), &memory));

        let last =
            unsafe { NonNull::new_unchecked(mapping.base().as_ptr().add(mapping.len() - 1)) };
        assert_eq!(map.get(mapping.base()), Some(id(10)));
        assert_eq!(map.get(last), Some(id(10)));

        unsafe { memory.unmap(mapping) };
    }
}
