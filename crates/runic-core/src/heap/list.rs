//! Owner-exclusive intrusive list.
//!
//! Doubly linked so `push_front`, `push_back`, and cursor `remove_current` are
//! O(1). One thread holds `&mut self`. Nodes are borrowed for `'a` (arena
//! slots), so pops return `&'a T` rather than a value this list owns.
//!
//! This is not [`super::queue::Queue`]. The queue is a singly linked front
//! stack sized for the run header. This list keeps a predecessor so a walk
//! can unlink one node.

use core::{cell::Cell, marker::PhantomData, ptr::NonNull};

/// Intrusive predecessor, successor, and membership.
pub(crate) struct Link<T> {
    next: Cell<Option<NonNull<T>>>,
    prev: Cell<Option<NonNull<T>>>,
    linked: Cell<bool>,
}

impl<T> Link<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: Cell::new(None),
            prev: Cell::new(None),
            linked: Cell::new(false),
        }
    }

    #[inline]
    pub(crate) fn is_linked(&self) -> bool {
        self.linked.get()
    }

    fn clear(&self) {
        self.next.set(None);
        self.prev.set(None);
        self.linked.set(false);
    }
}

/// Types that embed a [`Link`].
pub(crate) trait Linked: Sized {
    fn links(&self) -> &Link<Self>;
}

fn linked<'a, T: Linked>(ptr: NonNull<T>) -> &'a T {
    // SAFETY: every stored pointer came from `&'a T` while the node was linked.
    unsafe { ptr.as_ref() }
}

/// Owner-exclusive list of nodes borrowed for `'a`.
pub(crate) struct LinkedList<'a, T: Linked> {
    head: Option<NonNull<T>>,
    tail: Option<NonNull<T>>,
    len: usize,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> LinkedList<'a, T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
            marker: PhantomData,
        }
    }

    /// Insert `node` at the front. `node` must not already be linked.
    pub(crate) fn push_front(&mut self, node: &'a T) {
        debug_assert!(!node.links().is_linked());
        let ptr = NonNull::from(node);
        node.links().prev.set(None);
        node.links().next.set(self.head);
        node.links().linked.set(true);
        if let Some(head) = self.head {
            linked(head).links().prev.set(Some(ptr));
        } else {
            self.tail = Some(ptr);
        }
        self.head = Some(ptr);
        self.len += 1;
    }

    /// Insert `node` at the back. `node` must not already be linked.
    pub(crate) fn push_back(&mut self, node: &'a T) {
        debug_assert!(!node.links().is_linked());
        let ptr = NonNull::from(node);
        node.links().next.set(None);
        node.links().prev.set(self.tail);
        node.links().linked.set(true);
        if let Some(tail) = self.tail {
            linked(tail).links().next.set(Some(ptr));
        } else {
            self.head = Some(ptr);
        }
        self.tail = Some(ptr);
        self.len += 1;
    }

    /// Remove the front node.
    pub(crate) fn pop_front(&mut self) -> Option<&'a T> {
        let ptr = self.head?;
        let item = linked(ptr);
        let next = item.links().next.get();
        item.links().clear();
        self.head = next;
        if let Some(next) = next {
            linked(next).links().prev.set(None);
        } else {
            self.tail = None;
        }
        self.len -= 1;
        Some(item)
    }

    #[inline]
    pub(crate) fn front(&self) -> Option<&'a T> {
        self.head.map(linked)
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn iter(&self) -> Iter<'a, T> {
        Iter {
            next: self.head,
            marker: PhantomData,
        }
    }

    /// Cursor on the front node, or on the ghost when the list is empty.
    pub(crate) fn cursor_front_mut(&mut self) -> CursorMut<'a, '_, T> {
        CursorMut {
            current: self.head,
            list: self,
        }
    }
}

// SAFETY: `head` and `tail` point at arena slots of `T`. Mutation is `&mut self` only.
// `NonNull<T>` does not carry `T: Send`, so the list states it here.
unsafe impl<T: Linked + Send> Send for LinkedList<'_, T> {}
// SAFETY: sharing `&LinkedList` does not mutate the ends. Node links are the owner's.
unsafe impl<T: Linked + Sync> Sync for LinkedList<'_, T> {}

/// Forward iterator, front to back.
pub(crate) struct Iter<'a, T: Linked> {
    next: Option<NonNull<T>>,
    marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        let ptr = self.next?;
        let item = linked(ptr);
        self.next = item.links().next.get();
        Some(item)
    }
}

/// Editing cursor. On a non-empty list it starts on the front node.
pub(crate) struct CursorMut<'a, 'list, T: Linked> {
    current: Option<NonNull<T>>,
    list: &'list mut LinkedList<'a, T>,
}

impl<'a, T: Linked> CursorMut<'a, '_, T> {
    /// Node under the cursor, or `None` on the ghost.
    pub(crate) fn current(&self) -> Option<&'a T> {
        self.current.map(linked)
    }

    /// Advance. From the ghost this lands on the front node. From the back
    /// node this lands on the ghost.
    pub(crate) fn move_next(&mut self) {
        self.current = match self.current {
            None => self.list.head,
            Some(current) => linked(current).links().next.get(),
        };
    }

    /// Unlink the current node and land on its successor (or the ghost).
    pub(crate) fn remove_current(&mut self) -> Option<&'a T> {
        let current = self.current?;
        let item = linked(current);
        let next = item.links().next.get();
        let prev = item.links().prev.get();
        match prev {
            Some(prev) => linked(prev).links().next.set(next),
            None => self.list.head = next,
        }
        match next {
            Some(next) => linked(next).links().prev.set(prev),
            None => self.list.tail = prev,
        }
        item.links().clear();
        self.list.len -= 1;
        self.current = next;
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use super::*;

    struct TestNode {
        link: Link<TestNode>,
    }

    impl TestNode {
        fn new() -> Self {
            Self { link: Link::new() }
        }
    }

    impl Linked for TestNode {
        fn links(&self) -> &Link<Self> {
            &self.link
        }
    }

    #[test]
    fn push_front_then_pop_front_is_lifo() {
        let mut list = LinkedList::new();
        let older = TestNode::new();
        let newer = TestNode::new();
        list.push_front(&older);
        list.push_front(&newer);
        assert!(ptr::eq(
            ptr::from_ref(list.pop_front().unwrap()),
            ptr::from_ref(&newer)
        ));
        assert!(ptr::eq(
            ptr::from_ref(list.pop_front().unwrap()),
            ptr::from_ref(&older)
        ));
        assert!(list.pop_front().is_none());
        assert!(!older.link.is_linked());
    }

    #[test]
    fn push_back_keeps_front_stable() {
        let mut list = LinkedList::new();
        let first = TestNode::new();
        let second = TestNode::new();
        list.push_front(&first);
        list.push_back(&second);
        assert!(ptr::eq(
            ptr::from_ref(list.front().unwrap()),
            ptr::from_ref(&first)
        ));
        assert_eq!(list.len(), 2);
        let addrs: Vec<usize> = list.iter().map(|node| ptr::from_ref(node).addr()).collect();
        assert_eq!(
            addrs,
            [ptr::from_ref(&first).addr(), ptr::from_ref(&second).addr()]
        );
    }

    #[test]
    fn cursor_removes_the_middle_node() {
        let mut list = LinkedList::new();
        let first = TestNode::new();
        let middle = TestNode::new();
        let tail_node = TestNode::new();
        list.push_back(&first);
        list.push_back(&middle);
        list.push_back(&tail_node);

        let mut cursor = list.cursor_front_mut();
        cursor.move_next();
        assert!(ptr::eq(
            ptr::from_ref(cursor.remove_current().unwrap()),
            ptr::from_ref(&middle)
        ));
        assert!(!middle.link.is_linked());
        assert!(ptr::eq(
            ptr::from_ref(cursor.current().unwrap()),
            ptr::from_ref(&tail_node)
        ));

        let addrs: Vec<usize> = list.iter().map(|node| ptr::from_ref(node).addr()).collect();
        assert_eq!(
            addrs,
            [
                ptr::from_ref(&first).addr(),
                ptr::from_ref(&tail_node).addr()
            ]
        );
        assert_eq!(list.len(), 2);
    }
}
