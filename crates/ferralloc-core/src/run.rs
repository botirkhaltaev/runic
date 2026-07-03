use core::ptr::NonNull;

use crate::{
    address::AddressRange,
    free_list::FreeList,
    layout::LayoutSpec,
    os_memory::Mapping,
    size_class::{SizeClass, SizeClassId},
};

pub(crate) const RUN_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunId(u32);

impl RunId {
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockIndex(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    InvalidPointer,
    FreeUnderflow,
}

pub(crate) struct Run {
    id: RunId,
    mapping: Mapping,
    range: AddressRange,
    class: SizeClassId,
    block_size: usize,
    capacity: u32,
    live: u32,
    free: FreeList,
}

impl Run {
    pub(crate) fn new(id: RunId, mapping: Mapping, class: SizeClass) -> Self {
        let range = mapping.range();
        let block_size = class.block_size();
        let capacity = range.len().checked_div(block_size).unwrap_or(0);
        let mut run = Self {
            id,
            mapping,
            range,
            class: class.id(),
            block_size,
            capacity: u32::try_from(capacity).unwrap_or(u32::MAX),
            live: 0,
            free: FreeList::new(),
        };

        for index in 0..capacity {
            let Some(offset) = index.checked_mul(block_size) else {
                break;
            };
            // SAFETY: offset is within the run because index < range.len() / block_size.
            let raw_ptr = unsafe { range.base().as_ptr().add(offset) };
            // SAFETY: raw_ptr is derived from a non-null mapping base plus an in-bounds offset.
            let block_ptr = unsafe { NonNull::new_unchecked(raw_ptr) };
            // SAFETY: block_ptr points to writable memory in this run and is block-aligned.
            unsafe { run.free.push(block_ptr) };
        }

        run
    }

    pub(crate) const fn id(&self) -> RunId {
        self.id
    }

    pub(crate) const fn class(&self) -> SizeClassId {
        self.class
    }

    #[cfg(test)]
    pub(crate) fn base(&self) -> NonNull<u8> {
        self.range.base()
    }

    pub(crate) fn range(&self) -> AddressRange {
        debug_assert!(self.mapping.range().contains(self.range));

        self.range
    }

    pub(crate) fn allocate(&mut self, spec: LayoutSpec) -> Option<NonNull<u8>> {
        let ptr = self.free.pop()?;

        if !ptr.as_ptr().addr().is_multiple_of(spec.align()) {
            // SAFETY: ptr was just popped from this free list and is being restored unchanged.
            unsafe { self.free.push(ptr) };
            return None;
        }

        self.live = self.live.checked_add(1)?;
        Some(ptr)
    }

    pub(crate) fn free(&mut self, ptr: NonNull<u8>) -> Result<(), RunError> {
        let block = self.block_at(ptr).ok_or(RunError::InvalidPointer)?;

        // SAFETY: block_at validated that ptr is a block boundary in this run.
        unsafe { self.return_block(block) }
    }

    pub(crate) fn block_at(&self, ptr: NonNull<u8>) -> Option<BlockIndex> {
        let offset = self.range.offset_of(ptr)?;

        if !offset.is_multiple_of(self.block_size) {
            return None;
        }

        let index = offset.checked_div(self.block_size)?;
        let capacity = usize::try_from(self.capacity).ok()?;

        if index >= capacity {
            return None;
        }

        Some(BlockIndex(u32::try_from(index).ok()?))
    }

    unsafe fn return_block(&mut self, block: BlockIndex) -> Result<(), RunError> {
        let Some(live) = self.live.checked_sub(1) else {
            return Err(RunError::FreeUnderflow);
        };

        self.live = live;

        let Some(block_index) = usize::try_from(block.0).ok() else {
            return Err(RunError::InvalidPointer);
        };
        let Some(offset) = block_index.checked_mul(self.block_size) else {
            return Err(RunError::InvalidPointer);
        };
        // SAFETY: caller validated block against this run; offset is checked above.
        let raw_ptr = unsafe { self.range.base().as_ptr().add(offset) };
        // SAFETY: raw_ptr is derived from the non-null run base.
        let block_ptr = unsafe { NonNull::new_unchecked(raw_ptr) };
        // SAFETY: caller guarantees the block was allocated and belongs to this run.
        unsafe { self.free.push(block_ptr) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{layout::LayoutSpec, os_memory::OsMemory, size_class::SizeClasses};

    use super::*;

    fn class_for(size: usize, align: usize) -> SizeClass {
        let spec = LayoutSpec::from_size_align(size, align).unwrap();
        SizeClasses::get(spec).unwrap()
    }

    #[test]
    fn reusable_run_takes_each_block_once() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::new(0).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let capacity = RUN_SIZE / class.block_size();
        let mut seen = vec![false; capacity];

        for _ in 0..capacity {
            let ptr = run.allocate(spec).unwrap();
            let index = run.block_at(ptr).unwrap().0 as usize;

            assert!(!seen[index]);
            assert!(index < capacity);
            assert!((ptr.as_ptr() as usize) >= run.base().as_ptr() as usize);
            assert!((ptr.as_ptr() as usize) < run.base().as_ptr() as usize + RUN_SIZE);
            seen[index] = true;
        }

        assert!(run.allocate(spec).is_none());
        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn reusable_run_reuses_returned_block() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(128, 8);
        let mut run = Run::new(RunId::new(1).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(128, 8).unwrap();

        let ptr = run.allocate(spec).unwrap();
        let block = run.block_at(ptr).unwrap();

        assert_eq!(unsafe { run.return_block(block) }, Ok(()));

        assert_eq!(run.allocate(spec), Some(ptr));
    }

    #[test]
    fn reusable_run_rejects_interior_pointer() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::new(2).unwrap(), mapping, class);
        let spec = LayoutSpec::from_size_align(64, 8).unwrap();
        let ptr = run.allocate(spec).unwrap();
        let interior = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(1)) };

        assert!(run.block_at(interior).is_none());
    }

    #[test]
    fn reusable_run_return_block_reports_live_underflow() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let class = class_for(64, 8);
        let mut run = Run::new(RunId::new(7).unwrap(), mapping, class);
        let block = run.block_at(run.base()).unwrap();

        assert_eq!(
            unsafe { run.return_block(block) },
            Err(RunError::FreeUnderflow)
        );
    }

    #[test]
    fn reusable_run_returns_aligned_blocks_for_alignment_sensitive_layout() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let spec = LayoutSpec::from_size_align(17, 16).unwrap();
        let class = SizeClasses::get(spec).unwrap();
        let mut run = Run::new(RunId::new(3).unwrap(), mapping, class);
        let capacity = RUN_SIZE / class.block_size();

        for _ in 0..capacity {
            let ptr = run.allocate(spec).unwrap();
            assert_eq!(ptr.as_ptr() as usize % 16, 0);
        }
    }

    #[test]
    fn run_range_reports_mapping_range() {
        let mapping = OsMemory::map(RUN_SIZE).unwrap();
        let range = mapping.range();
        let run = Run::new(RunId::new(5).unwrap(), mapping, class_for(8, 8));

        assert_eq!(run.range().base(), range.base());
        assert_eq!(run.range().len(), range.len());
    }
}
