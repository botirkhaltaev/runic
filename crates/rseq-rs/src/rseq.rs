use core::{
    ptr::NonNull,
    sync::atomic::{AtomicIsize, AtomicU8, AtomicU32, Ordering},
};

use crate::{
    abi::{AREA_MIN, Area, CPU_UNINIT},
    cpus, membarrier,
    thread::{CpuId, Thread},
};

const UNSET: u8 = 0;
const READY: u8 = 1;
const FAIL: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNSET);
static OFFSET: AtomicIsize = AtomicIsize::new(0);
static CPUS: AtomicU32 = AtomicU32::new(0);

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
        match STATE.load(Ordering::Acquire) {
            READY => Some(Self::load()),
            FAIL => None,
            _ => init(),
        }
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

    fn load() -> Self {
        Self {
            offset: OFFSET.load(Ordering::Relaxed),
            cpus: CPUS.load(Ordering::Relaxed),
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn init() -> Option<Rseq> {
    let Some(rseq) = discover() else {
        STATE.store(FAIL, Ordering::Release);
        return None;
    };
    OFFSET.store(rseq.offset, Ordering::Relaxed);
    CPUS.store(rseq.cpus, Ordering::Relaxed);
    STATE.store(READY, Ordering::Release);
    Some(rseq)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn init() -> Option<Rseq> {
    STATE.store(FAIL, Ordering::Release);
    None
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
