use core::{alloc::Layout, ptr::NonNull};

use crate::{
    layout::LayoutSpec,
    os_memory::OsMemory,
    size_class::{SizeClass, SizeClasses},
    span::{SPAN_SIZE, Span, SpanSlot, SpanUse},
    span_map::SpanMap,
    span_table::SpanTable,
};

pub(crate) struct State {
    memory: OsMemory,
    classes: SizeClasses,
    spans: SpanTable,
    map: SpanMap,
    active: [SpanSlot; SizeClasses::COUNT],
}

unsafe impl Send for State {}

impl State {
    pub(crate) const fn new() -> Self {
        Self {
            memory: OsMemory::new(),
            classes: SizeClasses::new(),
            spans: SpanTable::new(),
            map: SpanMap::new(),
            active: [SpanSlot::empty(); SizeClasses::COUNT],
        }
    }

    pub(crate) fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let Some(spec) = LayoutSpec::from_layout(layout) else {
            return core::ptr::null_mut();
        };

        self.alloc_spec(spec)
    }

    pub(crate) fn dealloc(&mut self, ptr: *mut u8, _layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };

        let Some(id) = self.map.get(ptr) else {
            Self::abort();
        };

        let Some(kind) = self.spans.get(id).map(Span::use_kind) else {
            Self::abort();
        };

        match kind {
            SpanUse::Small => {
                let Some(span) = self.spans.get_mut(id) else {
                    Self::abort();
                };
                let Some(block) = span.block_index(ptr) else {
                    Self::abort();
                };
                if !unsafe { span.return_block(block) } {
                    Self::abort();
                }
            }
            SpanUse::Large => {
                let Some(span) = self.spans.get(id) else {
                    Self::abort();
                };
                let Some(mapping) = span.raw_mapping_for_large_ptr(ptr) else {
                    Self::abort();
                };
                let range = span.range();
                self.map.remove_range(range.base(), range.len());
                let removed = unsafe { self.spans.remove(id) };

                if removed.is_none() {
                    Self::abort();
                }

                unsafe { self.memory.unmap(mapping) };
            }
        }
    }

    pub(crate) fn realloc(&mut self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            let Some(spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
                return core::ptr::null_mut();
            };

            return self.alloc_spec(spec);
        }

        if new_size == 0 {
            self.dealloc(ptr, old);
            return core::ptr::null_mut();
        }

        let Some(old_ptr) = NonNull::new(ptr) else {
            return core::ptr::null_mut();
        };

        let Some(id) = self.map.get(old_ptr) else {
            Self::abort();
        };
        let Some(_span) = self.spans.get(id) else {
            Self::abort();
        };

        let Some(new_spec) = LayoutSpec::from_size_align(new_size, old.align()) else {
            return core::ptr::null_mut();
        };
        let new_ptr = self.alloc_spec(new_spec);

        if new_ptr.is_null() {
            return core::ptr::null_mut();
        }

        unsafe {
            core::ptr::copy_nonoverlapping(ptr, new_ptr, old.size().min(new_size));
        }

        self.dealloc(ptr, old);
        new_ptr
    }

    pub(crate) fn alloc_zeroed(&mut self, layout: Layout) -> *mut u8 {
        let ptr = self.alloc(layout);

        if !ptr.is_null() {
            unsafe {
                core::ptr::write_bytes(ptr, 0, layout.size());
            }
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
        let active = self.active[class.id().index()].get();

        if let Some(id) = active
            && let Some(span) = self.spans.get_mut(id)
            && let Some(ptr) = span.take_block(spec)
        {
            return ptr.as_ptr();
        }

        let Some(mapping) = self.memory.map(SPAN_SIZE) else {
            return core::ptr::null_mut();
        };
        let Some(id) = self.spans.reserve_id() else {
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        };

        let span = Span::small(id, mapping, class);
        debug_assert_eq!(span.id(), id);
        debug_assert_eq!(span.class_id(), Some(class.id()));
        let range = span.range();

        if !self.spans.insert(id, span) {
            self.spans.release_id(id);
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        }

        if !self
            .map
            .insert_range(range.base(), range.len(), id, &self.memory)
        {
            let _ = unsafe { self.spans.remove(id) };
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        }

        self.active[class.id().index()] = SpanSlot::some(id);

        let Some(span) = self.spans.get_mut(id) else {
            Self::abort();
        };

        span.take_block(spec)
            .map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    fn alloc_large(&mut self, spec: LayoutSpec) -> *mut u8 {
        let Some(len) = spec.large_mapping_len(self.memory.page_size()) else {
            return core::ptr::null_mut();
        };
        let Some(mapping) = self.memory.map(len) else {
            return core::ptr::null_mut();
        };
        let Some(id) = self.spans.reserve_id() else {
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        };
        let Some(span) = Span::large(id, mapping, spec) else {
            self.spans.release_id(id);
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        };
        debug_assert_eq!(span.id(), id);
        let ptr = span.base();
        let range = span.range();

        if !self.spans.insert(id, span) {
            self.spans.release_id(id);
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        }

        if !self
            .map
            .insert_range(range.base(), range.len(), id, &self.memory)
        {
            let _ = unsafe { self.spans.remove(id) };
            unsafe { self.memory.unmap(mapping) };
            return core::ptr::null_mut();
        }

        ptr.as_ptr()
    }

    #[cold]
    #[inline(never)]
    fn abort() -> ! {
        unsafe { libc::abort() }
    }
}
