use core::{
    hint::spin_loop,
    marker::PhantomData,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    layout::{self, Header, Region},
    quiesce::Quiesced,
    thread::CpuId,
};

/// Item that did not fit. Caller still owns the pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Full<T> {
    item: NonNull<T>,
}

impl<T> Full<T> {
    pub(crate) const fn new(item: NonNull<T>) -> Self {
        Self { item }
    }

    /// Pointer that was not stored.
    #[must_use]
    pub const fn item(self) -> NonNull<T> {
        self.item
    }
}

/// Shared pop / push / batch. `Token` is `()` here and [`crate::Thread`] on RSEQ stacks.
pub trait CpuStacks<T> {
    /// Per-backend identity (`()` or [`crate::Thread`]).
    type Token;

    /// Pop one pointer. `None` if empty or stopped.
    fn pop(&self, token: &Self::Token) -> Option<NonNull<T>>;

    /// Push one pointer.
    ///
    /// # Errors
    ///
    /// Returns [`Full`] when the current CPU stack is at capacity.
    fn push(&self, token: &Self::Token, item: NonNull<T>) -> Result<(), Full<T>>;

    /// Pop up to `out.len()` pointers. Returns how many were written.
    fn pop_batch(&self, token: &Self::Token, out: &mut [NonNull<T>]) -> usize;

    /// Push until full. Returns how many were stored.
    fn push_batch(&self, token: &Self::Token, items: &[NonNull<T>]) -> usize;
}

/// Per-CPU index stacks guarded by a TAS per CPU. Never used on the RSEQ hit.
pub struct LockedStacks<T> {
    region: Region,
    locks: NonNull<AtomicBool>,
    shift: u8,
    cap: u32,
    cpus: u32,
    _t: PhantomData<T>,
}

// SAFETY: `NonNull` is not `Send`/`Sync`; the TAS and mmap are process-private. `T` is moved, not shared.
unsafe impl<T: Send> Send for LockedStacks<T> {}
unsafe impl<T: Send> Sync for LockedStacks<T> {}

impl<T> LockedStacks<T> {
    /// Allocate per-CPU stacks. `None` if `cpus`/`cap` is zero or mmap fails.
    #[must_use]
    pub fn new(cpus: u32, cap: u32) -> Option<Self> {
        if cpus == 0 {
            return None;
        }
        let shift = layout::block_shift(cap)?;
        let extra = usize::try_from(cpus)
            .ok()?
            .checked_mul(size_of::<AtomicBool>())?;
        let len = layout::region_len(cpus, shift, extra)?;
        let region = Region::map(len)?;
        let block = 1usize.checked_shl(u32::from(shift))?;
        let slabs = usize::try_from(cpus).ok()?.checked_mul(block)?;
        // SAFETY: `slabs` is inside the mapping; lock array follows the slabs.
        let locks: NonNull<AtomicBool> =
            unsafe { NonNull::new_unchecked(region.base().as_ptr().add(slabs).cast()) };
        // SAFETY: we own the zeroed mapping.
        unsafe { layout::init_headers(region.base(), cpus, shift, cap) };
        for cpu in 0..cpus {
            // SAFETY: lock slot `cpu` is inside the extra tail.
            unsafe {
                locks
                    .as_ptr()
                    .add(cpu as usize)
                    .write(AtomicBool::new(false));
            }
        }
        Some(Self {
            region,
            locks,
            shift,
            cap,
            cpus,
            _t: PhantomData,
        })
    }

    /// Number of CPU slabs.
    #[must_use]
    pub const fn cpus(&self) -> u32 {
        self.cpus
    }

    /// Slots per CPU.
    #[must_use]
    pub const fn cap(&self) -> u32 {
        self.cap
    }

    /// Pop from `sched_getcpu`'s slab.
    #[must_use]
    pub fn pop(&self) -> Option<NonNull<T>> {
        self.pop_cpu(current_cpu()?)
    }

    /// Push onto `sched_getcpu`'s slab.
    ///
    /// # Errors
    ///
    /// Returns [`Full`] when that slab is at capacity or the CPU id is unknown.
    pub fn push(&self, item: NonNull<T>) -> Result<(), Full<T>> {
        let Some(cpu) = current_cpu() else {
            return Err(Full::new(item));
        };
        self.push_cpu(cpu, item)
    }

    /// Pop a batch from `sched_getcpu`'s slab.
    #[must_use]
    pub fn pop_batch(&self, out: &mut [NonNull<T>]) -> usize {
        let Some(cpu) = current_cpu() else {
            return 0;
        };
        self.pop_batch_cpu(cpu, out)
    }

    /// Push a batch onto `sched_getcpu`'s slab.
    #[must_use]
    pub fn push_batch(&self, items: &[NonNull<T>]) -> usize {
        let Some(cpu) = current_cpu() else {
            return 0;
        };
        self.push_batch_cpu(cpu, items)
    }

    /// Pop from a specific CPU slab.
    #[must_use]
    pub fn pop_cpu(&self, cpu: CpuId) -> Option<NonNull<T>> {
        let cpu = self.index(cpu)?;
        let _g = self.lock(cpu);
        // SAFETY: TAS held; header and slots are our mapping.
        unsafe { pop_unlocked(self.header(cpu)) }
    }

    /// Push onto a specific CPU slab.
    ///
    /// # Errors
    ///
    /// Returns [`Full`] when that slab is at capacity or `cpu` is out of range.
    pub fn push_cpu(&self, cpu: CpuId, item: NonNull<T>) -> Result<(), Full<T>> {
        let Some(cpu) = self.index(cpu) else {
            return Err(Full::new(item));
        };
        let _g = self.lock(cpu);
        // SAFETY: TAS held; header and slots are our mapping.
        unsafe { push_unlocked(self.header(cpu), item) }
    }

    /// Pop a batch from a specific CPU slab.
    #[must_use]
    pub fn pop_batch_cpu(&self, cpu: CpuId, out: &mut [NonNull<T>]) -> usize {
        let Some(cpu) = self.index(cpu) else {
            return 0;
        };
        let _g = self.lock(cpu);
        let mut n = 0;
        while n < out.len() {
            // SAFETY: TAS held.
            let Some(item) = (unsafe { pop_unlocked(self.header(cpu)) }) else {
                break;
            };
            out[n] = item;
            n += 1;
        }
        n
    }

    /// Exclusive drain of one CPU. TAS is held until [`Quiesced`] drops.
    #[must_use]
    pub fn quiesce(&self, cpu: CpuId) -> Option<Quiesced<'_, T>> {
        let id = self.index(cpu)?;
        let flag = self.acquire(id);
        let header = self.header(id);
        // SAFETY: TAS held.
        let current = unsafe { header.as_ref().current };
        unsafe {
            header.as_ptr().write(Header {
                current,
                capacity: 0,
            });
        }
        // SAFETY: exclusive until drop unlocks.
        Some(unsafe { Quiesced::new(header, self.cap, current, Some(flag)) })
    }

    /// Push a batch onto a specific CPU slab.
    #[must_use]
    pub fn push_batch_cpu(&self, cpu: CpuId, items: &[NonNull<T>]) -> usize {
        let Some(cpu) = self.index(cpu) else {
            return 0;
        };
        let _g = self.lock(cpu);
        let mut n = 0;
        while n < items.len() {
            // SAFETY: TAS held.
            if unsafe { push_unlocked(self.header(cpu), items[n]) }.is_err() {
                break;
            }
            n += 1;
        }
        n
    }

    fn index(&self, cpu: CpuId) -> Option<u32> {
        let id = cpu.get();
        (id < self.cpus).then_some(id)
    }

    fn header(&self, cpu: u32) -> NonNull<Header> {
        // SAFETY: `cpu` is in range for this mapping.
        unsafe { layout::header(self.region.base(), cpu, self.shift) }
    }

    fn lock(&self, cpu: u32) -> Guard<'_> {
        Guard(self.acquire(cpu))
    }

    fn acquire(&self, cpu: u32) -> &AtomicBool {
        // SAFETY: `cpu` < `cpus`; lock array is live for the region lifetime.
        let flag = unsafe { &*self.locks.as_ptr().add(cpu as usize) };
        while flag.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        flag
    }
}

impl<T> CpuStacks<T> for LockedStacks<T> {
    type Token = ();

    fn pop(&self, (): &()) -> Option<NonNull<T>> {
        LockedStacks::pop(self)
    }

    fn push(&self, (): &(), item: NonNull<T>) -> Result<(), Full<T>> {
        LockedStacks::push(self, item)
    }

    fn pop_batch(&self, (): &(), out: &mut [NonNull<T>]) -> usize {
        LockedStacks::pop_batch(self, out)
    }

    fn push_batch(&self, (): &(), items: &[NonNull<T>]) -> usize {
        LockedStacks::push_batch(self, items)
    }
}

struct Guard<'a>(&'a AtomicBool);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn current_cpu() -> Option<CpuId> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sched_getcpu` has no side effects on memory we own.
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu < 0 {
            return None;
        }
        CpuId::new(u32::try_from(cpu).ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// # Safety
/// Caller holds the CPU TAS and `hdr` is a live header.
unsafe fn pop_unlocked<T>(hdr: NonNull<Header>) -> Option<NonNull<T>> {
    // SAFETY: caller holds the CPU TAS and `hdr` is a live header.
    let header = unsafe { &mut *hdr.as_ptr() };
    if header.capacity == 0 || header.current == 0 {
        return None;
    }
    header.current -= 1;
    // SAFETY: `current` is in range.
    Some(unsafe { layout::slot::<T>(hdr, header.current).as_ptr().read() })
}

/// # Safety
/// Caller holds the CPU TAS and `hdr` is a live header.
unsafe fn push_unlocked<T>(hdr: NonNull<Header>, item: NonNull<T>) -> Result<(), Full<T>> {
    // SAFETY: caller holds the CPU TAS and `hdr` is a live header.
    let header = unsafe { &mut *hdr.as_ptr() };
    if header.current >= header.capacity {
        return Err(Full::new(item));
    }
    // SAFETY: `current` is below capacity.
    unsafe {
        layout::slot::<T>(hdr, header.current).as_ptr().write(item);
    }
    header.current += 1;
    Ok(())
}
