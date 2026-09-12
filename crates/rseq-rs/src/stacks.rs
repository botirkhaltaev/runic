use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    layout::{self, Region},
    locked::{CpuStacks, Full},
    rseq::Rseq,
    thread::Thread,
};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::x86_64;

pub(crate) enum Fast {
    Ok(*mut u8),
    Miss,
    Unavailable,
}

impl Fast {
    #[inline]
    pub(crate) fn from_status(status: u64, obj: *mut u8) -> Self {
        match status {
            0 => Self::Ok(obj),
            1 => Self::Miss,
            _ => Self::Unavailable,
        }
    }
}

enum Memory {
    Owned(Region),
    Borrowed(NonNull<u8>),
}

impl Memory {
    #[inline]
    fn base(&self) -> NonNull<u8> {
        match self {
            Self::Owned(region) => region.base(),
            Self::Borrowed(base) => *base,
        }
    }
}

/// Per-CPU index stacks. Hit methods take [`Thread`] so they do not reload TLS.
pub struct Stacks<T> {
    memory: Memory,
    rseq: Rseq,
    shift: u8,
    cap: u32,
    cpus: u32,
    _t: PhantomData<T>,
}

// SAFETY: `NonNull` is not `Send`/`Sync`; pop/push move `T` between threads.
unsafe impl<T: Send> Send for Stacks<T> {}
unsafe impl<T: Send> Sync for Stacks<T> {}

impl<T> Stacks<T> {
    /// Crate-owned mapping sized for `rseq.cpus()` slabs.
    #[must_use]
    pub fn new(rseq: Rseq, cap: u32) -> Option<Self> {
        let cpus = rseq.cpus();
        if cpus == 0 {
            return None;
        }
        let shift = layout::block_shift(cap)?;
        let len = layout::region_len(cpus, shift, 0)?;
        let region = Region::map(len)?;
        // SAFETY: we own the zeroed mapping.
        unsafe { layout::init_headers(region.base(), cpus, shift, cap) };
        Some(Self {
            memory: Memory::Owned(region),
            rseq,
            shift,
            cap,
            cpus,
            _t: PhantomData,
        })
    }

    /// Use a caller-owned region with this crate's header + slot layout.
    ///
    /// # Safety
    ///
    /// `base` is live for `cpus` blocks of `1 << shift` bytes, used only as
    /// this crate's header and pointer slots for `T`, and outlives `Self`.
    #[must_use]
    pub unsafe fn from_raw(rseq: Rseq, base: NonNull<u8>, shift: u8, cap: u32, cpus: u32) -> Self {
        Self {
            memory: Memory::Borrowed(base),
            rseq,
            shift,
            cap,
            cpus,
            _t: PhantomData,
        }
    }

    #[must_use]
    pub const fn rseq(&self) -> Rseq {
        self.rseq
    }

    #[must_use]
    pub const fn cpus(&self) -> u32 {
        self.cpus
    }

    #[must_use]
    pub const fn cap(&self) -> u32 {
        self.cap
    }

    /// Pop from the CPU in `thread`.
    #[inline]
    #[must_use]
    pub fn pop(&self, thread: &Thread) -> Option<NonNull<T>> {
        match self.fast_pop(thread) {
            Fast::Ok(ptr) => NonNull::new(ptr.cast()),
            Fast::Miss | Fast::Unavailable => None,
        }
    }

    /// Push onto the CPU in `thread`.
    ///
    /// # Errors
    ///
    /// Returns [`Full`] when that slab is at capacity or rseq cannot commit.
    #[inline]
    pub fn push(&self, thread: &Thread, item: NonNull<T>) -> Result<(), Full<T>> {
        match self.fast_push(thread, item) {
            Fast::Ok(_) => Ok(()),
            Fast::Miss | Fast::Unavailable => Err(Full::new(item)),
        }
    }

    /// Pop a batch from the CPU in `thread`.
    #[must_use]
    pub fn pop_batch(&self, thread: &Thread, out: &mut [NonNull<T>]) -> usize {
        let mut n = 0;
        while n < out.len() {
            let Some(item) = self.pop(thread) else {
                break;
            };
            out[n] = item;
            n += 1;
        }
        n
    }

    /// Push a batch onto the CPU in `thread`.
    #[must_use]
    pub fn push_batch(&self, thread: &Thread, items: &[NonNull<T>]) -> usize {
        let mut n = 0;
        while n < items.len() {
            if self.push(thread, items[n]).is_err() {
                break;
            }
            n += 1;
        }
        n
    }

    #[inline]
    fn fast_pop(&self, thread: &Thread) -> Fast {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // SAFETY: `thread` is bound; `base` is our mapping or `from_raw` region.
            unsafe {
                x86_64::pop(
                    thread.area(),
                    self.memory.base().as_ptr(),
                    self.shift,
                    self.cpus,
                )
            }
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = thread;
            Fast::Unavailable
        }
    }

    #[inline]
    fn fast_push(&self, thread: &Thread, item: NonNull<T>) -> Fast {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // SAFETY: `thread` is bound; `base` is our mapping or `from_raw` region.
            unsafe {
                x86_64::push(
                    thread.area(),
                    self.memory.base().as_ptr(),
                    self.shift,
                    self.cpus,
                    item.as_ptr().cast(),
                )
            }
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (thread, item);
            Fast::Unavailable
        }
    }
}

impl<T> CpuStacks<T> for Stacks<T> {
    type Token = Thread;

    fn pop(&self, token: &Thread) -> Option<NonNull<T>> {
        Stacks::pop(self, token)
    }

    fn push(&self, token: &Thread, item: NonNull<T>) -> Result<(), Full<T>> {
        Stacks::push(self, token, item)
    }

    fn pop_batch(&self, token: &Thread, out: &mut [NonNull<T>]) -> usize {
        Stacks::pop_batch(self, token, out)
    }

    fn push_batch(&self, token: &Thread, items: &[NonNull<T>]) -> usize {
        Stacks::push_batch(self, token, items)
    }
}

impl Rseq {
    /// Allocate crate-owned per-CPU stacks.
    #[must_use]
    pub fn stacks<T>(self, cap: u32) -> Option<Stacks<T>> {
        Stacks::new(self, cap)
    }
}
