//! Intrusive Treiber stack shared by [`super::Mpsc`] and [`super::Mpmc`].
//!
//! `push` stores `next` with Release before the head CAS publishes the node.
//! `pop` and [`super::Mpsc::drain`] Acquire-load `next` only after reading the
//! head word that published it. Callers do not wrap this in their own atomics.

use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

/// Intrusive `next` for the shared stack. Embed this on the node.
pub(crate) struct Link<T> {
    next: AtomicPtr<T>,
}

impl<T> Link<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Address `addr` with no provenance. Inbox idle and pushing sentinels use this.
    pub(crate) const fn dangling(addr: usize) -> Self {
        Self {
            next: AtomicPtr::new(ptr::without_provenance_mut(addr)),
        }
    }

    pub(crate) fn store(&self, next: *mut T) {
        self.next.store(next, Ordering::Release);
    }

    pub(crate) fn load(&self) -> *mut T {
        self.next.load(Ordering::Acquire)
    }

    pub(crate) fn cas(&self, current: *mut T, next: *mut T) -> Result<*mut T, *mut T> {
        self.next
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
    }
}

/// Types that embed a [`Link`] for [`Stack`].
pub(crate) trait Linked: Sized {
    fn links(&self) -> &Link<Self>;
}

/// Multi-producer stack. `push` and `pop` are concurrent. `detach` hands the
/// whole chain to one walker.
pub(super) struct Stack<'a, T: Linked> {
    head: AtomicPtr<T>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> Stack<'a, T> {
    pub(super) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            marker: PhantomData,
        }
    }

    /// Link `node` in front of the head. `node` must not already be on a stack.
    pub(super) fn push(&self, node: &'a T) {
        let raw = ptr::from_ref(node).cast_mut();
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            node.links().store(head);
            match self
                .head
                .compare_exchange_weak(head, raw, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }

    /// Pop the newest node. Concurrent with `push` and other `pop`s.
    pub(super) fn pop(&self) -> Option<&'a T> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let node = NonNull::new(head)?;
            // SAFETY: `push` stored `next` before the CAS that published `node`.
            let next = unsafe { node.as_ref() }.links().load();
            match self
                .head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
            {
                // SAFETY: `push` stored this pointer from `&'a T` before the CAS.
                Ok(_) => return Some(unsafe { node.as_ref() }),
                Err(current) => head = current,
            }
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Detach every node. The caller walks `links` and must not race another detach
    /// on this chain. Concurrent `push` either lands in the detached chain or stays
    /// on `head`.
    pub(super) fn detach(&self) -> Option<NonNull<T>> {
        NonNull::new(self.head.swap(ptr::null_mut(), Ordering::AcqRel))
    }
}
