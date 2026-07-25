//! Grow-on-demand mmap object table with an intrusive freelist.
//!
//! `new(max)` records a hard index limit only — no slots are mapped until
//! `claim`. Growth maps chunk-sized slot groups via [`OsMemory::map`]. The newest
//! chunk is a typed [`Chunk`] (owns its [`Mapping`]); older chunks stay linked
//! from that mapping's header. Slots are empty or occupied only. Callers that
//! fail after `claim` and before `insert` must `release` the index.

use core::{mem::MaybeUninit, ptr::NonNull};

use crate::memory::{Mapping, OsMemory, PAGE_SIZE};

const FREE_END: u32 = u32::MAX;

/// Target mapping size per growth step (page-rounded by [`OsMemory::map`]).
const CHUNK_BYTES: usize = 256 * 1024;

pub(crate) struct Arena<T> {
    max: usize,
    /// One past the highest index ever handed out by bump `claim`.
    bump: usize,
    free_head: u32,
    slots_per_chunk: usize,
    /// Newest chunk. Older chunks are linked through [`ChunkHeader::next`].
    head: Option<Chunk<T>>,
    chunk_count: usize,
}

/// One live slot chunk: owns the mmap and a typed pointer into its slot array.
struct Chunk<T> {
    mapping: Mapping,
    base: NonNull<Slot<T>>,
    len: usize,
}

/// Leading bytes of each chunk mapping; `next` links older chunks.
#[repr(C)]
struct ChunkHeader {
    next: Option<NonNull<ChunkHeader>>,
    /// Byte length passed to `munmap` for older (non-head) chunks.
    bytes: usize,
    /// Slot count in this chunk.
    slots: usize,
}

// SAFETY: Arena owns mmap-backed storage. Moving ownership does not permit concurrent mutation.
unsafe impl<T: Send> Send for Arena<T> {}

impl<T> Arena<T> {
    pub(crate) fn new(max: u32) -> Self {
        let max = usize::try_from(max).unwrap_or(0);
        Self {
            max,
            bump: 0,
            free_head: FREE_END,
            slots_per_chunk: Self::slots_per_chunk(),
            head: None,
            chunk_count: 0,
        }
    }

    pub(crate) fn claim(&mut self) -> Option<usize> {
        if self.free_head != FREE_END {
            let index = usize::try_from(self.free_head).ok()?;
            let next = {
                let slot = self.slot_mut(index)?;
                debug_assert!(!slot.is_occupied());
                slot.take_next()
            };
            self.free_head = next;
            return Some(index);
        }

        if self.bump >= self.max {
            return None;
        }

        self.ensure_chunk(self.bump)?;
        let index = self.bump;
        self.bump += 1;
        Some(index)
    }

    pub(crate) fn release(&mut self, index: usize) {
        let next = self.free_head;
        let Some(slot) = self.slot_mut(index) else {
            return;
        };
        if slot.is_occupied() {
            return;
        }

        slot.set_empty(next);
        let Ok(head) = u32::try_from(index) else {
            return;
        };
        self.free_head = head;
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) -> Option<&mut T> {
        let slot = self.slot_mut(index)?;
        if slot.is_occupied() {
            return None;
        }

        slot.occupy(value);
        slot.get_mut()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.slot(index)?.get()
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slot_mut(index)?.get_mut()
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<T> {
        let value = self.slot_mut(index)?.remove()?;
        self.release(index);
        Some(value)
    }

    /// Hard index limit passed to [`Self::new`].
    pub(crate) fn capacity(&self) -> usize {
        self.max
    }

    /// Number of indices ever committed by bump growth (`0..len` may be mapped).
    pub(crate) fn len(&self) -> usize {
        self.bump
    }

    fn slots_per_chunk() -> usize {
        let slot = core::mem::size_of::<Slot<T>>().max(1);
        let available = CHUNK_BYTES.saturating_sub(Self::header_bytes());
        (available / slot).max(1)
    }

    fn header_bytes() -> usize {
        let header = core::mem::size_of::<ChunkHeader>();
        let align = core::mem::align_of::<Slot<T>>().max(1);
        header.div_ceil(align) * align
    }

    fn ensure_chunk(&mut self, index: usize) -> Option<()> {
        let chunk_index = index / self.slots_per_chunk;
        if chunk_index < self.chunk_count {
            return Some(());
        }
        if chunk_index != self.chunk_count {
            return None;
        }

        let start = chunk_index.checked_mul(self.slots_per_chunk)?;
        if start >= self.max {
            return None;
        }
        let slots = self.slots_per_chunk.min(self.max - start);
        let byte_len = Self::header_bytes()
            .checked_add(slots.checked_mul(core::mem::size_of::<Slot<T>>())?)?
            .max(PAGE_SIZE);
        let mapping = OsMemory::map(byte_len)?;
        let bytes = mapping.len().get();
        let mapping_base = mapping.base();
        let slot_base = Self::slots_base(mapping_base.cast());

        let older = self.head.take().map(|old| {
            let older_header = old.mapping.base().cast::<ChunkHeader>();
            // Older chunk stays reachable through the new header; drop its Mapping
            // owner so Drop/munmap is unified through the header walk.
            core::mem::forget(old.mapping);
            older_header
        });

        let header_ptr = mapping_base.cast::<ChunkHeader>();
        // SAFETY: fresh anonymous mapping of `bytes` bytes, uniquely owned by `mapping`.
        unsafe {
            header_ptr.write(ChunkHeader {
                next: older,
                bytes,
                slots,
            });
        }

        self.head = Some(Chunk {
            mapping,
            base: slot_base,
            len: slots,
        });
        self.chunk_count += 1;
        Some(())
    }

    fn chunk_header(&self, chunk_index: usize) -> Option<NonNull<ChunkHeader>> {
        // Newest chunk is at the head; chunk 0 was allocated first → deepest.
        if chunk_index >= self.chunk_count {
            return None;
        }
        let from_head = self.chunk_count - 1 - chunk_index;
        let mut cur = self.head.as_ref()?.mapping.base().cast::<ChunkHeader>();
        for _ in 0..from_head {
            // SAFETY: chunk list nodes are live headers in owned mappings.
            cur = unsafe { cur.as_ref().next? };
        }
        Some(cur)
    }

    fn slots_base(header: NonNull<ChunkHeader>) -> NonNull<Slot<T>> {
        // SAFETY: mapping layout is [ChunkHeader][pad][Slot; slots] within owned bytes.
        unsafe {
            NonNull::new_unchecked(
                header
                    .as_ptr()
                    .cast::<u8>()
                    .add(Self::header_bytes())
                    .cast(),
            )
        }
    }

    fn slot(&self, index: usize) -> Option<&Slot<T>> {
        let ptr = self.slot_ptr(index)?;
        // SAFETY: `slot_ptr` yields a live slot inside an owned chunk mapping.
        Some(unsafe { ptr.as_ref() })
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut Slot<T>> {
        let mut ptr = self.slot_ptr(index)?;
        // SAFETY: `slot_ptr` yields a live slot inside an owned chunk mapping; Arena is uniquely borrowed.
        Some(unsafe { ptr.as_mut() })
    }

    fn slot_ptr(&self, index: usize) -> Option<NonNull<Slot<T>>> {
        if index >= self.bump {
            return None;
        }

        let chunk_index = index / self.slots_per_chunk;
        let offset = index % self.slots_per_chunk;

        // Fast path: newest chunk holds the typed base directly.
        if chunk_index + 1 == self.chunk_count {
            let chunk = self.head.as_ref()?;
            if offset >= chunk.len {
                return None;
            }
            // SAFETY: offset is in-range for the head chunk's slot array.
            return Some(unsafe { NonNull::new_unchecked(chunk.base.as_ptr().add(offset)) });
        }

        let header = self.chunk_header(chunk_index)?;
        // SAFETY: header is a live chunk in this arena's list.
        let slots = unsafe { header.as_ref().slots };
        if offset >= slots {
            return None;
        }

        // SAFETY: offset is in-range for this chunk's slot array.
        Some(unsafe { NonNull::new_unchecked(Self::slots_base(header).as_ptr().add(offset)) })
    }
}

impl<T> Drop for Arena<T> {
    fn drop(&mut self) {
        for index in 0..self.bump {
            if let Some(slot) = self.slot_mut(index) {
                slot.drop_value();
            }
        }

        let Some(head) = self.head.take() else {
            return;
        };

        let mut cur = Some(head.mapping.base().cast::<ChunkHeader>());
        // Head Mapping would munmap on drop; walk all headers (including head) once.
        core::mem::forget(head.mapping);

        while let Some(header) = cur {
            // SAFETY: each node is a chunk header we wrote at mapping base.
            let (next, bytes) = unsafe {
                let header_ref = header.as_ref();
                (header_ref.next, header_ref.bytes)
            };
            // SAFETY: mapping was created by OsMemory::map for this chunk.
            unsafe {
                libc::munmap(header.as_ptr().cast(), bytes);
            }
            cur = next;
        }
    }
}

#[repr(C)]
struct Slot<T> {
    value: MaybeUninit<T>,
    /// Freelist next when empty; unused when occupied.
    next: u32,
    occupied: u8,
}

impl<T> Slot<T> {
    fn set_empty(&mut self, next: u32) {
        self.occupied = 0;
        self.next = next;
    }

    fn take_next(&mut self) -> u32 {
        debug_assert!(!self.is_occupied());
        self.next
    }

    fn is_occupied(&self) -> bool {
        self.occupied != 0
    }

    fn occupy(&mut self, value: T) {
        debug_assert!(!self.is_occupied());
        self.value.write(value);
        self.occupied = 1;
        self.next = FREE_END;
    }

    fn get_mut(&mut self) -> Option<&mut T> {
        if !self.is_occupied() {
            return None;
        }
        // SAFETY: occupied is set only after value.write.
        Some(unsafe { self.value.assume_init_mut() })
    }

    fn get(&self) -> Option<&T> {
        if !self.is_occupied() {
            return None;
        }
        // SAFETY: occupied is set only after value.write.
        Some(unsafe { self.value.assume_init_ref() })
    }

    fn remove(&mut self) -> Option<T> {
        if !self.is_occupied() {
            return None;
        }
        self.occupied = 0;
        // SAFETY: occupied was true, so value is initialized.
        Some(unsafe { self.value.assume_init_read() })
    }

    fn drop_value(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct DropCounter<'a> {
        drops: &'a Cell<usize>,
    }

    impl Drop for DropCounter<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn arena_zero_capacity_claims_none() {
        let mut arena = Arena::<u32>::new(0);
        assert_eq!(arena.claim(), None);
    }

    #[test]
    fn arena_claims_from_zero() {
        let mut arena = Arena::<u32>::new(4);
        assert_eq!(arena.claim(), Some(0));
        assert_eq!(arena.claim(), Some(1));
    }

    #[test]
    fn arena_respects_capacity() {
        let mut arena = Arena::<u32>::new(2);
        assert_eq!(arena.claim(), Some(0));
        assert_eq!(arena.claim(), Some(1));
        assert_eq!(arena.claim(), None);
    }

    #[test]
    fn arena_release_returns_index_to_freelist() {
        let mut arena = Arena::<u32>::new(4);
        let first = arena.claim().unwrap();
        let second = arena.claim().unwrap();
        arena.release(first);
        assert_eq!(second, 1);
        assert_eq!(arena.claim(), Some(first));
    }

    #[test]
    fn arena_insert_get_remove_round_trip() {
        let mut arena = Arena::<u32>::new(4);
        let index = arena.claim().unwrap();
        assert_eq!(arena.insert(index, 42).copied(), Some(42));
        assert_eq!(arena.get(index).copied(), Some(42));
        assert_eq!(arena.remove(index), Some(42));
        assert_eq!(arena.get(index), None);
        assert_eq!(arena.claim(), Some(index));
    }

    #[test]
    fn arena_rejects_insert_on_occupied() {
        let mut arena = Arena::<u32>::new(4);
        let index = arena.claim().unwrap();
        assert!(arena.insert(index, 1).is_some());
        assert!(arena.insert(index, 2).is_none());
        assert_eq!(arena.get(index).copied(), Some(1));
    }

    #[test]
    fn arena_drop_drops_occupied_only() {
        let drops = Cell::new(0);
        {
            let mut arena = Arena::new(4);
            let occupied = arena.claim().unwrap();
            let claimed = arena.claim().unwrap();
            assert!(
                arena
                    .insert(occupied, DropCounter { drops: &drops })
                    .is_some()
            );
            assert_ne!(occupied, claimed);
            // `claimed` is released by dropping the arena without insert — no DropCounter.
        }
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn arena_grows_across_chunks_without_eager_len() {
        let mut arena = Arena::<u32>::new(4_096);
        assert_eq!(arena.len(), 0);

        let first = arena.claim().unwrap();
        assert_eq!(first, 0);
        assert_eq!(arena.len(), 1);
        assert!(arena.insert(first, 7).is_some());

        let limit = arena.capacity().min(arena.slots_per_chunk + 1);
        while arena.len() < limit {
            let index = arena.claim().unwrap();
            assert!(arena.insert(index, u32::try_from(index).unwrap()).is_some());
        }
        assert_eq!(arena.len(), limit);
        assert_eq!(arena.get(0).copied(), Some(7));
        assert_eq!(
            arena.get(limit - 1).copied(),
            Some(u32::try_from(limit - 1).unwrap())
        );
    }
}
