use core::ptr::NonNull;
use std::sync::OnceLock;

use crate::{
    abi::{AREA_MIN, Area, CPU_UNINIT},
    cpus, membarrier,
    thread::{CpuId, Thread},
};

static STATE: OnceLock<Option<Rseq>> = OnceLock::new();

/// Process-wide rseq registration. `Copy`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rseq {
    offset: isize,
    cpus: u32,
}

impl Rseq {
    /// glibc-registered area, possible CPU count, and RSEQ membarrier.
    /// `None` if rseq or membarrier is unavailable. `#[cold]`; call once.
    #[cold]
    #[must_use]
    pub fn try_new() -> Option<Self> {
        *STATE.get_or_init(init)
    }

    /// Bind this thread's area. `#[cold]`; store the `Thread` in caller TLS.
    #[cold]
    #[must_use]
    pub fn bind(self) -> Option<Thread> {
        let area = thread_area(self.offset)?;
        // SAFETY: `area` is the glibc-registered rseq TLS for this thread.
        let id = unsafe { area.as_ref().cpu_id };
        if id == CPU_UNINIT || id >= self.cpus {
            return None;
        }
        Some(Thread::new(area))
    }

    /// Expedited RSEQ membarrier targeted at `cpu`.
    #[must_use]
    pub fn fence(self, cpu: CpuId) -> bool {
        membarrier::fence(cpu.get())
    }

    #[must_use]
    pub const fn cpus(self) -> u32 {
        self.cpus
    }
}

fn init() -> Option<Rseq> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        discover()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        None
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn discover() -> Option<Rseq> {
    let size = usize::try_from(unsafe { libc_rseq_size() }).ok()?;
    if size < AREA_MIN {
        return None;
    }
    // SAFETY: glibc publishes `__rseq_offset` for every thread.
    let offset = unsafe { libc_rseq_offset() };
    let cpus = cpus::possible()?;
    if cpus == 0 {
        return None;
    }
    if !membarrier::register() {
        return None;
    }
    Some(Rseq { offset, cpus })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe extern "C" {
    static __rseq_offset: libc::ptrdiff_t;
    static __rseq_size: libc::c_uint;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn libc_rseq_offset() -> libc::ptrdiff_t {
    // SAFETY: glibc publishes `__rseq_offset` for every thread.
    unsafe { __rseq_offset }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn libc_rseq_size() -> libc::c_uint {
    // SAFETY: glibc publishes `__rseq_size` (0 if rseq is off).
    unsafe { __rseq_size }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn thread_area(offset: isize) -> Option<NonNull<Area>> {
    let tp = thread_pointer();
    NonNull::new(tp.wrapping_offset(offset).cast())
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn thread_area(_offset: isize) -> Option<NonNull<Area>> {
    None
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn thread_pointer() -> *mut u8 {
    let tp: *mut u8;
    // SAFETY: `fs:0` is the x86_64 thread pointer.
    unsafe {
        core::arch::asm!(
            "mov {}, fs:0",
            out(reg) tp,
            options(nostack, preserves_flags, readonly, pure)
        );
    }
    tp
}
