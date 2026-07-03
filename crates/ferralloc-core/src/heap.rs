use core::{
    alloc::Layout,
    ptr::{NonNull, copy_nonoverlapping, null_mut, write_bytes},
};

use crate::{
    extent::Extent,
    extent_table::{ExtentReservation, ExtentTable},
    layout::LayoutSpec,
    os_memory::OsMemory,
    page_map::{PageEntry, PageMap, PageRange},
    run::{RUN_SIZE, Run, RunId},
    run_table::{RunReservation, RunTable},
    size_class::{SizeClass, SizeClasses},
};

pub(crate) struct Heap {
    memory: OsMemory,
    classes: SizeClasses,
    runs: RunTable,
    extents: ExtentTable,
    pages: PageMap,
    active: [Option<RunId>; SizeClasses::COUNT],
}

impl Heap {
    pub(crate) const fn new() -> Self {
        Self {
            memory: OsMemory::new(),
            classes: SizeClasses::new(),
            runs: RunTable::new(),
            extents: ExtentTable::new(),
            pages: PageMap::new(),
            active: [None; SizeClasses::COUNT],
        }
    }

    pub(crate) fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let spec = LayoutSpec::from_layout(layout);

        self.alloc_spec(spec)
    }

    pub(crate) fn dealloc(&mut self, raw_ptr: *mut u8, _layout: Layout) {
        let Some(ptr) = NonNull::new(raw_ptr) else {
            return;
        };

        let Some(entry) = self.pages.get(ptr) else {
            Self::abort();
        };

        match entry {
            PageEntry::Run(id) => {
                let class_index = {
                    let Some(run) = self.runs.get_mut(id) else {
                        Self::abort();
                    };

                    if run.free(ptr).is_err() {
                        Self::abort();
                    }

                    run.class().index()
                };

                let Some(active_slot) = self.active.get_mut(class_index) else {
                    Self::abort();
                };
                *active_slot = Some(id);
            }
            PageEntry::Extent(id) => {
                let range = {
                    let Some(extent) = self.extents.get(id) else {
                        Self::abort();
                    };

                    if extent.free(ptr).is_err() {
                        Self::abort();
                    }

                    extent.range()
                };

                let Some(page_range) = PageRange::from_range(range) else {
                    Self::abort();
                };
                self.pages.remove(page_range);

                if self.extents.remove(id).is_none() {
                    Self::abort();
                }
            }
        }
    }

    pub(crate) fn realloc(&mut self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            let Some(spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
                return null_mut();
            };

            return self.alloc_spec(spec);
        }

        if new_size == 0 {
            self.dealloc(ptr, old);
            return null_mut();
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return null_mut();
        };

        let Some(entry) = self.pages.get(old_ptr) else {
            Self::abort();
        };
        match entry {
            PageEntry::Run(id) => {
                if self.runs.get(id).is_none() {
                    Self::abort();
                }
            }
            PageEntry::Extent(id) => {
                if self.extents.get(id).is_none() {
                    Self::abort();
                }
            }
        }

        let Some(new_spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
            return null_mut();
        };
        let new_ptr = self.alloc_spec(new_spec);

        if new_ptr.is_null() {
            return null_mut();
        }

        // SAFETY: new_ptr is a fresh allocation of at least new_size bytes; ptr is valid for old.size().
        unsafe { copy_nonoverlapping(ptr, new_ptr, old.size().min(new_size)) };

        self.dealloc(ptr, old);
        new_ptr
    }

    pub(crate) fn alloc_zeroed(&mut self, layout: Layout) -> *mut u8 {
        let ptr = self.alloc(layout);

        if !ptr.is_null() {
            // SAFETY: ptr is valid for layout.size() bytes because it was just allocated for layout.
            unsafe { write_bytes(ptr, 0, layout.size()) };
        }

        ptr
    }

    fn alloc_spec(&mut self, spec: LayoutSpec) -> *mut u8 {
        match self.classes.get(spec) {
            Some(class) => self.alloc_small(spec, class),
            None => self.alloc_large(spec),
        }
    }

    fn alloc_small(&mut self, spec: LayoutSpec, class: SizeClass) -> *mut u8 {
        let class_index = class.id().index();
        let active_id = self.active.get(class_index).copied().flatten();

        if let Some(id) = active_id
            && let Some(run) = self.runs.get_mut(id)
            && let Some(ptr) = run.allocate(spec)
        {
            return ptr.as_ptr();
        }

        if let Some((id, ptr)) = self.runs.allocate(class, spec) {
            let Some(active_slot) = self.active.get_mut(class_index) else {
                Self::abort();
            };
            *active_slot = Some(id);

            return ptr.as_ptr();
        }

        let Some(mapping) = self.memory.map(RUN_SIZE) else {
            return null_mut();
        };
        let Some(reservation) = self.runs.reserve(&self.memory) else {
            return null_mut();
        };
        let id = reservation.id();

        let run = Run::new(id, mapping, class);
        if self.insert_run(reservation, run).is_err() {
            return null_mut();
        }

        let Some(active_slot) = self.active.get_mut(class_index) else {
            Self::abort();
        };
        *active_slot = Some(id);

        let Some(inserted_run) = self.runs.get_mut(id) else {
            Self::abort();
        };

        inserted_run
            .allocate(spec)
            .map_or(null_mut(), NonNull::as_ptr)
    }

    fn alloc_large(&mut self, spec: LayoutSpec) -> *mut u8 {
        let Some(len) = spec.mapping_len(self.memory.page_size()) else {
            return null_mut();
        };
        let Some(mapping) = self.memory.map(len) else {
            return null_mut();
        };
        let Some(reservation) = self.extents.reserve(&self.memory) else {
            return null_mut();
        };
        let id = reservation.id();
        let Some(extent) = Extent::new(id, mapping, spec) else {
            self.extents.release(reservation);
            return null_mut();
        };
        debug_assert_eq!(extent.id(), id, "new extent should keep its reserved id");
        let ptr = extent.ptr();

        if self.insert_extent(reservation, extent).is_err() {
            return null_mut();
        }

        ptr.as_ptr()
    }

    fn insert_run(&mut self, reservation: RunReservation, run: Run) -> Result<RunId, ()> {
        let id = reservation.id();
        let range = run.range();

        if self.runs.insert(reservation, run).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::from_range(range) else {
            let _removed = self.runs.remove(id);
            return Err(());
        };

        if self
            .pages
            .insert(page_range, PageEntry::Run(id), &self.memory)
            .is_err()
        {
            let _removed = self.runs.remove(id);
            return Err(());
        }

        Ok(id)
    }

    fn insert_extent(&mut self, reservation: ExtentReservation, extent: Extent) -> Result<(), ()> {
        let id = reservation.id();
        let range = extent.range();

        if self.extents.insert(reservation, extent).is_err() {
            return Err(());
        }

        let Some(page_range) = PageRange::from_range(range) else {
            let _removed = self.extents.remove(id);
            return Err(());
        };

        if self
            .pages
            .insert(page_range, PageEntry::Extent(id), &self.memory)
            .is_err()
        {
            let _removed = self.extents.remove(id);
            return Err(());
        }

        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        // SAFETY: abort terminates the process and does not unwind across allocator boundaries.
        unsafe { libc::abort() }
    }
}
