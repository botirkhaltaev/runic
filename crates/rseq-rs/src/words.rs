use core::{num::NonZeroUsize, ptr::NonNull};

use crate::{
    layout::{self, Region},
    rseq::Rseq,
    thread::CpuId,
};

/// One `usize` and the CPU it belongs to. librseq's `(v, cpu)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Word {
    ptr: NonNull<usize>,
    cpu: CpuId,
}

impl Word {
    /// Caller-owned `usize` paired with `cpu`.
    ///
    /// # Safety
    ///
    /// `ptr` is a live `usize`, used only as this word, and outlives the ops.
    #[must_use]
    pub const unsafe fn from_raw(ptr: NonNull<usize>, cpu: CpuId) -> Self {
        Self { ptr, cpu }
    }

    #[must_use]
    pub const fn cpu(self) -> CpuId {
        self.cpu
    }

    #[must_use]
    pub const fn as_ptr(self) -> NonNull<usize> {
        self.ptr
    }
}

/// Optional mmap of one `usize` per possible CPU.
pub struct Words {
    base: NonNull<u8>,
    len: Option<NonZeroUsize>,
    cpus: u32,
}

// SAFETY: `NonNull` is not `Send`/`Sync`; the words are process-private usizes.
unsafe impl Send for Words {}
unsafe impl Sync for Words {}

impl Words {
    /// Allocate `cpus` words. `None` if `cpus` is zero or mmap fails.
    #[must_use]
    pub fn new(cpus: u32) -> Option<Self> {
        if cpus == 0 {
            return None;
        }
        let len = layout::region_len(cpus)?;
        let region = Region::map(len)?;
        let (base, len) = region.into_raw();
        Some(Self {
            base,
            len: Some(len),
            cpus,
        })
    }

    /// Use a caller-owned region of `cpus` words.
    ///
    /// # Safety
    ///
    /// `base` is live for `cpus` `usize`s, used only as this crate's per-CPU
    /// words, and outlives `Self`.
    #[must_use]
    pub unsafe fn from_raw(base: NonNull<u8>, cpus: u32) -> Self {
        Self {
            base,
            len: None,
            cpus,
        }
    }

    /// Words in the region.
    #[must_use]
    pub const fn cpus(&self) -> u32 {
        self.cpus
    }

    /// Pointer plus `cpu`. Address math, not a critical section.
    #[must_use]
    pub fn get(&self, cpu: CpuId) -> Option<Word> {
        let id = cpu.get();
        if id >= self.cpus {
            return None;
        }
        // SAFETY: `id` is in range for this mapping.
        let ptr = unsafe { layout::word(self.base, id) };
        Some(Word { ptr, cpu })
    }
}

impl Rseq {
    /// Allocate crate-owned per-CPU words.
    #[must_use]
    pub fn words(self) -> Option<Words> {
        Words::new(self.cpus())
    }
}

impl Drop for Words {
    fn drop(&mut self) {
        if let Some(len) = self.len {
            // SAFETY: `len` is set only for mappings created by `Region::map`.
            unsafe {
                libc::munmap(self.base.as_ptr().cast(), len.get());
            }
        }
    }
}
