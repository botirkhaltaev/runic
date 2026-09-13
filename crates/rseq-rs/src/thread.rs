use core::ptr::{self, NonNull};

use crate::abi::{Area, CPU_REG_FAILED, CPU_UNINIT};
use crate::words::Word;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::x86_64;

/// Compare-miss or a CPU-mismatch abort. Kernel preemption restarts inside the CS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The word was not `expect`. Carries the value that was seen.
    Miss(usize),
    /// This thread is not on `word.cpu`. Re-read [`Thread::cpu_id`] and pick a
    /// new word. Do not retry the same [`Word`].
    Abort,
}

/// Logical CPU index. Newtype so it cannot be mixed with a raw `u32`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CpuId(u32);

impl CpuId {
    /// Rejects the kernel uninitialized and registration-failed sentinels.
    #[must_use]
    pub const fn new(id: u32) -> Option<Self> {
        if id == CPU_UNINIT || id == CPU_REG_FAILED {
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
///
/// Call [`crate::Rseq::bind`] again in a child after `fork`. The process
/// registration stays valid; this pointer does not.
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

    #[inline]
    pub(crate) fn cpu_id_of(area: NonNull<Area>) -> u32 {
        // SAFETY: `area` is a registered rseq TLS; the kernel writes `cpu_id`.
        unsafe { ptr::addr_of!((*area.as_ptr()).cpu_id).read_volatile() }
    }

    /// Kernel `cpu_id`. `None` if unregistered or a sentinel.
    #[must_use]
    pub fn cpu_id(&self) -> Option<CpuId> {
        CpuId::new(Self::cpu_id_of(self.area()))
    }

    /// Compare `word` to `expect` and store `new`.
    ///
    /// This is librseq `cmpeqv_storev`, not [`core::sync::atomic::AtomicUsize::compare_exchange`].
    /// `Abort` is a CPU mismatch, not a spurious CAS failure.
    ///
    /// One attempt. Kernel preemption restarts inside the CS. CPU mismatch
    /// is [`Error::Abort`] — re-read [`Self::cpu_id`] and pick a new word.
    ///
    /// # Errors
    ///
    /// [`Error::Miss`] when the word is not `expect`. [`Error::Abort`] when
    /// this thread is not on `word.cpu`, or rseq is unavailable on this target.
    #[inline]
    pub fn compare_exchange(
        &self,
        word: Word<'_>,
        expect: usize,
        new: usize,
    ) -> Result<usize, Error> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // SAFETY: `self` is bound; `word` is a live AtomicUsize for `word.cpu`.
            // The CS is a Relaxed atomic RMW; rseq is atomicity vs same-CPU threads.
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
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (word, expect, new);
            Err(Error::Abort)
        }
    }

    /// Add `count` to `word`. Returns the previous value.
    ///
    /// One attempt. Kernel preemption restarts inside the CS. CPU mismatch
    /// is [`Error::Abort`] — re-read [`Self::cpu_id`] and pick a new word.
    ///
    /// # Errors
    ///
    /// [`Error::Abort`] when this thread is not on `word.cpu`, or rseq is
    /// unavailable on this target.
    #[inline]
    pub fn fetch_add(&self, word: Word<'_>, count: usize) -> Result<usize, Error> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // SAFETY: `self` is bound; `word` is a live AtomicUsize for `word.cpu`.
            // The CS is a Relaxed atomic RMW; rseq is atomicity vs same-CPU threads.
            match unsafe {
                x86_64::fetch_add(self.area(), word.as_ptr().as_ptr(), word.cpu().get(), count)
            } {
                x86_64::Attempt::Ok(prev) => Ok(prev),
                x86_64::Attempt::Miss(_) | x86_64::Attempt::Abort => Err(Error::Abort),
            }
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (word, count);
            Err(Error::Abort)
        }
    }
}
