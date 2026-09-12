use core::ptr::NonNull;

use crate::abi::{Area, CPU_UNINIT};
use crate::words::Word;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::x86_64;

/// Compare-miss or a CPU-mismatch abort. Kernel preemption restarts inside the CS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The word was not `expect`. Carries the value that was seen.
    Miss(usize),
    /// This thread is not on `word.cpu`. Re-read [`Thread::cpu_id`] and pick again.
    Abort,
}

/// Logical CPU index. Newtype so it cannot be mixed with a raw `u32`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CpuId(u32);

impl CpuId {
    /// Rejects the kernel uninitialized sentinel.
    #[must_use]
    pub const fn new(id: u32) -> Option<Self> {
        if id == CPU_UNINIT {
            None
        } else {
            Some(Self(id))
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// This thread's registered `struct rseq`. `Copy`. `*mut` so it is not `Send`.
#[derive(Clone, Copy, Debug)]
pub struct Thread {
    area: *mut Area,
}

impl Thread {
    pub(crate) const fn new(area: NonNull<Area>) -> Self {
        Self {
            area: area.as_ptr(),
        }
    }

    #[inline]
    pub(crate) const fn area(&self) -> NonNull<Area> {
        // SAFETY: constructed from `NonNull`.
        unsafe { NonNull::new_unchecked(self.area) }
    }

    /// Kernel `cpu_id`. `None` if unregistered or a sentinel.
    #[must_use]
    pub fn cpu_id(&self) -> Option<CpuId> {
        // SAFETY: `area` is this thread's registered rseq TLS.
        let id = unsafe { (*self.area).cpu_id };
        CpuId::new(id)
    }

    /// Compare `word` to `expect` and store `new`.
    ///
    /// One attempt. Kernel preemption restarts inside the CS. CPU mismatch
    /// is [`Error::Abort`] — re-read [`Self::cpu_id`] and pick a new word.
    ///
    /// # Errors
    ///
    /// [`Error::Miss`] when the word is not `expect`. [`Error::Abort`] when
    /// this thread is not on `word.cpu`.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[inline]
    pub fn compare_exchange(
        &self,
        word: Word<'_>,
        expect: usize,
        new: usize,
    ) -> Result<usize, Error> {
        // SAFETY: `self` is bound; `word` is a live usize for `word.cpu`.
        match unsafe {
            x86_64::compare_exchange(
                self.area(),
                word.as_ptr().as_ptr(),
                word.cpu().get(),
                expect,
                new,
            )
        } {
            x86_64::Attempt::Ok(old) => Ok(old),
            x86_64::Attempt::Miss(current) => Err(Error::Miss(current)),
            x86_64::Attempt::Abort => Err(Error::Abort),
        }
    }

    /// Add `count` to `word`. Returns the previous value.
    ///
    /// One attempt. Kernel preemption restarts inside the CS. CPU mismatch
    /// is [`Error::Abort`] — re-read [`Self::cpu_id`] and pick a new word.
    ///
    /// # Errors
    ///
    /// [`Error::Abort`] when this thread is not on `word.cpu`.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[inline]
    pub fn fetch_add(&self, word: Word<'_>, count: usize) -> Result<usize, Error> {
        // SAFETY: `self` is bound; `word` is a live usize for `word.cpu`.
        match unsafe {
            x86_64::fetch_add(self.area(), word.as_ptr().as_ptr(), word.cpu().get(), count)
        } {
            x86_64::Attempt::Ok(prev) => Ok(prev),
            x86_64::Attempt::Miss(_) | x86_64::Attempt::Abort => Err(Error::Abort),
        }
    }
}
