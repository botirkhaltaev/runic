use core::ptr::NonNull;

use crate::abi::{Area, CPU_UNINIT};

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
        Self { area: area.as_ptr() }
    }

    /// Kernel `cpu_id`. `None` if unregistered or a sentinel.
    #[must_use]
    pub fn cpu_id(self) -> Option<CpuId> {
        // SAFETY: `area` is this thread's registered rseq TLS.
        let id = unsafe { (*self.area).cpu_id };
        CpuId::new(id)
    }
}
