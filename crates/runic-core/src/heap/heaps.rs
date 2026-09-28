use core::{hint, num::NonZeroU32, ptr::NonNull};

use crate::{arena::Arena, config::AllocatorConfig, heap::HeapError, memory::PageOwner};

use super::queue::Mpmc;
use super::{AllocatorCtx, Heap, HeapId, HeapInner, OwnerState};

/// Indexes heaps. [`Arena<Heap>`] is the published directory; Free heaps are an
/// [`Mpmc`] stack. Pushed from [`Heap::reclaim`].
pub(crate) struct Heaps {
    arena: Arena<Heap>,
    free: Mpmc<'static, Heap>,
    config: AllocatorConfig,
}

impl Heaps {
    pub(crate) const fn new(config: AllocatorConfig) -> Self {
        Self {
            arena: Arena::new(),
            free: Mpmc::new(),
            config,
        }
    }

    /// Acquire a heap for TLS bind: pop a Free heap or claim a fresh one.
    pub(crate) fn acquire(&self) -> Option<&Heap> {
        if let Some(heap) = self.reuse() {
            return Some(heap);
        }
        let (_, heap) = self.arena.push(|index| {
            let id = HeapId::new(index, NonZeroU32::MIN)?;
            Some(Heap::new(id, self.config))
        })?;
        Some(heap)
    }

    fn reuse(&self) -> Option<&Heap> {
        loop {
            let heap = self.free.pop()?;
            if heap.state.is_retired() || !heap.state.is_free() {
                continue;
            }
            heap.reactivate();
            return Some(heap);
        }
    }

    /// Link a just-reclaimed Free heap. Caller holds Inner.
    ///
    /// The slot outlives this borrow: arena entries are process-lifetime.
    pub(super) fn push_free(&self, heap: &Heap) {
        // SAFETY: `heap` is an arena slot. Slots are not moved or freed, so the
        // pointer stays valid until a later `pop` on this stack.
        let heap = unsafe { &*core::ptr::from_ref(heap) };
        self.free.push(heap);
    }

    /// Generation-checked shared borrow. Lock-free directory read.
    pub(crate) fn get(&self, id: HeapId) -> Option<&Heap> {
        let heap = self.arena.get(id.index())?;
        heap.matches(id).then_some(heap)
    }

    /// Late free while Draining. Then reclaim if this owner emptied.
    pub(crate) fn free(
        &self,
        id: HeapId,
        owner: PageOwner,
        ptr: NonNull<u8>,
        ctx: &AllocatorCtx,
    ) -> Result<(), HeapError> {
        let (heap, mut inner) = self.admit(id, Some(owner))?;
        let state = inner.free(owner, ptr, ctx.pages)?;
        if state == OwnerState::Empty {
            #[cfg(feature = "hardened")]
            heap.release_delay(&mut inner, ctx.pages)?;
            heap.reclaim(&inner, self);
        }
        Ok(())
    }

    /// Accept inboxes while Draining. `owner` enqueues a claimed remote first.
    pub(crate) fn flush(
        &self,
        id: HeapId,
        ctx: &AllocatorCtx,
        owner: Option<PageOwner>,
    ) -> Result<(), HeapError> {
        let (heap, mut inner) = self.admit(id, owner)?;
        heap.flush(&mut inner, ctx, owner)?;
        heap.reclaim(&inner, self);
        Ok(())
    }

    /// Inner while Draining. `owner` uses `owner.heap()`; `None` looks up `id`.
    fn admit(
        &self,
        id: HeapId,
        owner: Option<PageOwner>,
    ) -> Result<(&Heap, spin::MutexGuard<'_, HeapInner>), HeapError> {
        let heap = match owner {
            Some(owner) => owner.heap(),
            None => self.get(id).ok_or(HeapError::InvalidHeap)?,
        };
        Ok((heap, heap.admit(id, owner)?))
    }

    /// Owner gives up the heap: close Active, wait leases, reclaim, flush.
    pub(crate) fn unbind(&self, id: HeapId, ctx: &AllocatorCtx) -> Result<(), HeapError> {
        {
            let Some(heap) = self.get(id) else {
                return Ok(());
            };
            heap.close(id)?;
        }

        self.wait_leases(id);

        match self.flush(id, ctx, None) {
            Ok(()) | Err(HeapError::InvalidHeap) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn wait_leases(&self, id: HeapId) {
        let mut spins = 0u32;
        loop {
            let Some(heap) = self.get(id) else {
                return;
            };
            if heap.leases() == 0 {
                return;
            }
            hint::spin_loop();
            spins = spins.saturating_add(1);
            if spins == 64 {
                spins = 0;
                // SAFETY: `sched_yield` has no pointer or ownership contract.
                unsafe { libc::sched_yield() };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use std::sync::{Barrier, mpsc};
    use std::thread;

    use super::*;
    use crate::{
        heap::HeapMode,
        layout::LayoutSpec,
        memory::{PageMap, PageOwner},
    };

    #[test]
    fn acquire_unbind_reactivate_bumps_generation() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let first = heaps.acquire().unwrap().id();
        assert_eq!(first.generation().get(), 1);
        let pages = PageMap::new();
        assert_eq!(
            heaps.unbind(
                first,
                &AllocatorCtx {
                    pages: &pages,
                    heaps: &heaps
                }
            ),
            Ok(())
        );
        assert!(heaps.get(first).is_none());

        let second = heaps.acquire().unwrap().id();
        assert_eq!(second.index(), first.index());
        assert_eq!(second.generation().get(), 2);
        assert!(heaps.get(second).is_some());
        assert!(heaps.get(first).is_none());
    }

    #[test]
    fn cached_extent_derives_reactivated_generation() {
        static HEAPS: OnceLock<Heaps> = OnceLock::new();
        static PAGES: OnceLock<PageMap> = OnceLock::new();

        let heaps = HEAPS.get_or_init(|| Heaps::new(AllocatorConfig::new()));
        let pages = PAGES.get_or_init(PageMap::new);
        let ctx = AllocatorCtx { pages, heaps };
        let heap = heaps.acquire().unwrap();
        let first = heap.id();
        let spec = LayoutSpec::from_layout(Layout::from_size_align(128 * 1024, 4096).unwrap());
        let ptr = {
            let mut inner = heap.require_inner();
            inner
                .extents
                .allocate(spec, heap, pages, crate::heap::ExtentInit::Uninit)
                .unwrap()
                .unwrap()
        };
        let Some(PageOwner::Extent(extent)) = pages.get(ptr) else {
            panic!("expected extent owner");
        };
        {
            let mut inner = heap.require_inner();
            inner.extents.free(extent, ptr, pages).unwrap();
        }

        assert_eq!(heaps.unbind(first, &ctx), Ok(()));
        let second = heaps.acquire().unwrap().id();
        assert_eq!(second.index(), first.index());
        assert_ne!(second.generation(), first.generation());
        assert_eq!(extent.heap().id(), second);
        assert_eq!(heaps.unbind(second, &ctx), Ok(()));
    }

    #[test]
    fn generation_exhaustion_permanently_retires_heap() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let id = heaps.acquire().unwrap().id();
        let index = id.index();
        let max_gen = NonZeroU32::new(u32::MAX).unwrap();
        let heap = heaps.get(id).unwrap();
        heap.state.store(max_gen, HeapMode::Draining, 0);
        let id_max = HeapId::new(index, max_gen).unwrap();
        let inner = heap.inner.lock();
        assert!(heap.reclaim(&inner, &heaps));
        drop(inner);
        assert!(heaps.get(id).is_none());
        assert!(heaps.get(id_max).is_none());
        assert!(heaps.arena.get(index).unwrap().state.is_retired());
        let other = heaps.acquire().unwrap().id();
        assert_ne!(other.index(), id.index());
    }

    #[test]
    fn unbind_waits_for_in_flight_lease() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let id = heaps.acquire().unwrap().id();
        let heap = heaps.get(id).unwrap();
        let lease = heap.state.lease(id).unwrap();
        let pages = PageMap::new();
        let start = Barrier::new(2);
        let (done_tx, done_rx) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                assert_eq!(
                    heaps.unbind(
                        id,
                        &AllocatorCtx {
                            pages: &pages,
                            heaps: &heaps
                        }
                    ),
                    Ok(())
                );
                done_tx.send(()).unwrap();
            });

            start.wait();
            // Observe Draining with the lease still held — no wall-clock probe.
            while heap.state.mode() != HeapMode::Draining {
                hint::spin_loop();
            }
            assert_eq!(heap.state.leases(), 1);
            assert!(done_rx.try_recv().is_err());
            drop(lease);
            done_rx.recv().unwrap();
        });

        assert!(heaps.get(id).is_none());
    }

    #[test]
    fn adopt_and_reclaim_cannot_both_win() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let id = heaps.acquire().unwrap().id();
        let heap = heaps.get(id).unwrap();
        assert_eq!(heap.close(id), Ok(()));
        let start = Barrier::new(3);
        let adopted = AtomicBool::new(false);

        thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                if heap.adopt(id).is_ok() {
                    adopted.store(true, Ordering::Release);
                }
            });
            scope.spawn(|| {
                start.wait();
                let inner = heap.inner.lock();
                heap.reclaim(&inner, &heaps);
            });
            start.wait();
        });

        let pages = PageMap::new();
        let ctx = AllocatorCtx {
            pages: &pages,
            heaps: &heaps,
        };
        if adopted.load(Ordering::Acquire) {
            assert_eq!(heaps.get(id).map(Heap::mode), Some(HeapMode::Active));
            assert_eq!(heaps.unbind(id, &ctx), Ok(()));
        } else {
            assert!(heaps.get(id).is_none());
        }
    }

    #[test]
    fn acquire_grows_past_sixty_four_live_heaps() {
        const LIVE: usize = 96;
        let heaps = Heaps::new(AllocatorConfig::new());
        let pages = PageMap::new();
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..LIVE {
                scope.spawn(|| {
                    tx.send(heaps.acquire().unwrap().id()).unwrap();
                });
            }
        });
        drop(tx);
        let ids: Vec<_> = rx.iter().collect();
        assert_eq!(ids.len(), LIVE);

        let mut indexes: Vec<u32> = ids.iter().map(|id| id.index()).collect();
        indexes.sort_unstable();
        let unique = indexes.len();
        indexes.dedup();
        assert_eq!(indexes.len(), unique);

        for id in ids {
            assert_eq!(
                heaps.unbind(
                    id,
                    &AllocatorCtx {
                        pages: &pages,
                        heaps: &heaps,
                    }
                ),
                Ok(())
            );
        }

        let reused = heaps.acquire().unwrap().id();
        assert!(reused.index() < u32::try_from(LIVE).unwrap());
    }

    #[test]
    fn get_sees_published_heaps() {
        let heaps = Heaps::new(AllocatorConfig::new());
        let n = 32;
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..n {
                    tx.send(heaps.acquire().unwrap().id()).unwrap();
                }
            });
            let mut seen = 0usize;
            for _ in 0..n {
                let id = rx.recv().unwrap();
                while heaps.get(id).is_none() {
                    hint::spin_loop();
                }
                seen += 1;
            }
            assert_eq!(seen, n);
        });
    }
}
