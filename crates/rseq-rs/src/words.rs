use core::{marker::PhantomData, ptr::NonNull, sync::atomic::AtomicUsize};

use crate::{
    layout::{self, Region},
    thread::CpuId,
};

/// One word and the CPU it belongs to. librseq's `(v, cpu)`.
///
/// [`Words::get`] borrows the region. [`Word::from_raw`] is `'static`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Word<'a> {
    ptr: NonNull<AtomicUsize>,
    cpu: CpuId,
    _a: PhantomData<&'a AtomicUsize>,
}

impl Word<'static> {
    /// Caller-owned word paired with `cpu`.
    ///
    /// # Safety
    ///
    /// `ptr` is a live aligned [`AtomicUsize`], used only as this word, and
    /// outlives the ops.
    #[must_use]
    pub const unsafe fn from_raw(ptr: NonNull<AtomicUsize>, cpu: CpuId) -> Self {
        Self {
            ptr,
            cpu,
            _a: PhantomData,
        }
    }
}

impl Word<'_> {
    #[must_use]
    pub const fn cpu(self) -> CpuId {
        self.cpu
    }

    #[must_use]
    pub const fn as_ptr(self) -> NonNull<AtomicUsize> {
        self.ptr
    }
}

/// Optional mmap of one word per possible CPU.
pub struct Words {
    backing: Backing,
    cpus: u32,
}

enum Backing {
    Mapped(Region),
    Raw(NonNull<u8>),
}

impl Backing {
    const fn base(&self) -> NonNull<u8> {
        match self {
            Self::Mapped(region) => region.base(),
            Self::Raw(base) => *base,
        }
    }
}

// SAFETY: the mapping is process-private `AtomicUsize`s. `NonNull<u8>` is
// `!Send`/`!Sync`; the words are the atomics the CS and drain already share.
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
        Some(Self {
            backing: Backing::Mapped(Region::map(len)?),
            cpus,
        })
    }

    /// Use a caller-owned region of `cpus` words.
    ///
    /// # Safety
    ///
    /// `base` is live for `cpus` aligned [`AtomicUsize`]s (`cpus > 0`), used
    /// only as this crate's per-CPU words, and outlives `Self`.
    #[must_use]
    pub unsafe fn from_raw(base: NonNull<u8>, cpus: u32) -> Self {
        debug_assert!(cpus > 0);
        Self {
            backing: Backing::Raw(base),
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
    pub fn get(&self, cpu: CpuId) -> Option<Word<'_>> {
        let id = cpu.get();
        if id >= self.cpus {
            return None;
        }
        // SAFETY: `id` is in range for this mapping.
        let ptr = unsafe { layout::word(self.backing.base(), id) };
        Some(Word {
            ptr,
            cpu,
            _a: PhantomData,
        })
    }
}
