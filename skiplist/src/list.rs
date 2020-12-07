use super::arena::Arena;
use super::KeyComparator;
use super::MAX_HEIGHT;
use bytes::Bytes;
use rand::Rng;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::{mem, ptr, u32};

const HEIGHT_INCREASE: u32 = u32::MAX / 3;

// Uses C layout to make sure tower is at the bottom
#[derive(Debug)]
#[repr(C)]
pub struct Node {
    key: Bytes,
    value: Bytes,
    height: usize,
    tower: [AtomicPtr<Self>; MAX_HEIGHT as usize],
}

impl Node {
    fn alloc(arena: &Arena, key: Bytes, value: Bytes, height: usize) -> *mut Node {
        let align = mem::align_of::<Node>();
        let size = mem::size_of::<Node>();
        // Not all values in Node::tower will be utilized.
        let not_used =
            (MAX_HEIGHT as usize - height as usize - 1) * mem::size_of::<AtomicPtr<Self>>();
        let node_ptr = arena.alloc::<Self>(align, size - not_used);
        unsafe {
            // let node_ptr: *mut Node = arena.get_mut(node_ptr);
            let node = &mut *node_ptr;
            ptr::write(&mut node.key, key);
            ptr::write(&mut node.value, value);
            node.height = height;
            ptr::write_bytes(node.tower.as_mut_ptr(), 0, height + 1);
        }
        node_ptr
    }

    fn next_mut(&self, height: usize) -> *mut Node {
        self.tower[height].load(Ordering::SeqCst)
    }
}

struct SkiplistCore {
    height: AtomicUsize,
    head: NonNull<Node>,
    arena: Arena,
}

#[derive(Clone)]
pub struct Skiplist<C> {
    core: Arc<SkiplistCore>,
    c: C,
}

impl<C> Skiplist<C> {
    pub fn with_capacity(c: C, arena_size: u32) -> Skiplist<C> {
        let arena = Arena::with_capacity(arena_size);
        let head_ptr = Node::alloc(&arena, Bytes::new(), Bytes::new(), MAX_HEIGHT - 1);
        let head = unsafe { NonNull::new_unchecked(head_ptr) };
        Skiplist {
            core: Arc::new(SkiplistCore {
                height: AtomicUsize::new(0),
                head,
                arena,
            }),
            c,
        }
    }

    fn random_height(&self) -> usize {
        let mut rng = rand::thread_rng();
        for h in 0..(MAX_HEIGHT - 1) {
            if !rng.gen_ratio(HEIGHT_INCREASE, u32::MAX) {
                return h;
            }
        }
        MAX_HEIGHT - 1
    }

    fn height(&self) -> usize {
        self.core.height.load(Ordering::SeqCst)
    }
}

impl<C: KeyComparator> Skiplist<C> {
    unsafe fn find_near(&self, key: &[u8], less: bool, allow_equal: bool) -> *const Node {
        let mut cursor: *const Node = self.core.head.as_ptr();
        let mut level = self.height();
        loop {
            let next_ptr = (&*cursor).next_mut(level);
            if next_ptr.is_null() {
                if level > 0 {
                    level -= 1;
                    continue;
                }
                if !less || cursor == self.core.head.as_ptr() {
                    return ptr::null();
                }
                return cursor;
            }
            let next = &*next_ptr;
            let res = self.c.compare_key(key, &next.key);
            if res == std::cmp::Ordering::Greater {
                cursor = next_ptr;
                continue;
            }
            if res == std::cmp::Ordering::Equal {
                if allow_equal {
                    return next;
                }
                if !less {
                    return next.next_mut(0);
                }
                if level > 0 {
                    level -= 1;
                    continue;
                }
                if cursor == self.core.head.as_ptr() {
                    return ptr::null();
                }
                return cursor;
            }
            if level > 0 {
                level -= 1;
                continue;
            }
            if !less {
                return next;
            }
            if cursor == self.core.head.as_ptr() {
                return ptr::null();
            }
            return cursor;
        }
    }

    unsafe fn find_splice_for_level(
        &self,
        key: &[u8],
        mut before: *mut Node,
        level: usize,
    ) -> (*mut Node, *mut Node) {
        loop {
            let next_ptr = (&*before).next_mut(level);
            if next_ptr.is_null() {
                return (before, ptr::null_mut());
            }
            let next_node = &*next_ptr;
            match self.c.compare_key(&key, &next_node.key) {
                std::cmp::Ordering::Equal => return (next_ptr, next_ptr),
                std::cmp::Ordering::Less => return (before, next_ptr),
                _ => before = next_ptr,
            }
        }
    }

    pub fn put(&self, key: impl Into<Bytes>, value: impl Into<Bytes>) -> Option<(Bytes, Bytes)> {
        let (key, value) = (key.into(), value.into());
        let mut list_height = self.height();
        let mut prev = [ptr::null_mut(); MAX_HEIGHT + 1];
        let mut next = [ptr::null_mut(); MAX_HEIGHT + 1];
        prev[list_height + 1] = self.core.head.as_ptr();
        next[list_height + 1] = ptr::null_mut();
        for i in (0..=list_height).rev() {
            let (p, n) = unsafe { self.find_splice_for_level(&key, prev[i + 1], i) };
            prev[i] = p;
            next[i] = n;
            if p == n {
                unsafe {
                    if (*p).value != value {
                        return Some((key, value));
                    }
                }
                return None;
            }
        }

        let height = self.random_height();
        let x = unsafe { &mut *Node::alloc(&self.core.arena, key, value, height) };
        while height > list_height {
            match self.core.height.compare_exchange_weak(
                list_height,
                height,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(h) => list_height = h,
            }
        }
        for i in 0..=height {
            loop {
                if prev[i].is_null() {
                    assert!(i > 1);
                    let (p, n) =
                        unsafe { self.find_splice_for_level(&x.key, self.core.head.as_ptr(), i) };
                    prev[i] = p;
                    next[i] = n;
                    assert_ne!(p, n);
                }
                x.tower[i].store(next[i], Ordering::SeqCst);
                match unsafe { &*prev[i] }.tower[i].compare_exchange(
                    next[i],
                    x,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(_) => {
                        let (p, n) = unsafe { self.find_splice_for_level(&x.key, prev[i], i) };
                        if p == n {
                            assert_eq!(i, 0);
                            if unsafe { &*p }.value != x.value {
                                let key = mem::replace(&mut x.key, Bytes::new());
                                let value = mem::replace(&mut x.value, Bytes::new());
                                return Some((key, value));
                            }
                            unsafe {
                                ptr::drop_in_place(x);
                            }
                            return None;
                        }
                        prev[i] = p;
                        next[i] = n;
                    }
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        let node = self.core.head.as_ptr();
        unsafe { (&*node).next_mut(0) }.is_null()
    }

    pub fn len(&self) -> usize {
        let mut node = self.core.head.as_ptr();
        let mut count = 0;
        loop {
            let next = unsafe { (&*node).next_mut(0) };
            if !next.is_null() {
                count += 1;
                node = next;
                continue;
            }
            return count;
        }
    }

    fn find_last(&self) -> *const Node {
        let mut node = self.core.head.as_ptr();
        let mut level = self.height();
        loop {
            let next = unsafe { (&*node).next_mut(level) };
            if !next.is_null() {
                node = next;
                continue;
            }
            if level == 0 {
                if node == self.core.head.as_ptr() {
                    return ptr::null();
                }
                return node;
            }
            level -= 1;
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&Bytes> {
        if let Some((_, value)) = self.get_with_key(key) {
            Some(value)
        } else {
            None
        }
    }

    pub fn get_with_key(&self, key: &[u8]) -> Option<(&Bytes, &Bytes)> {
        let node = unsafe { self.find_near(key, false, true) };
        if node.is_null() {
            return None;
        }
        if self.c.same_key(&unsafe { &*node }.key, key) {
            return Some(unsafe { (&(*node).key, &(*node).value) });
        }
        None
    }

    pub fn iter_ref(&self) -> IterRef<'_, C> {
        IterRef {
            list: self,
            cursor: ptr::null(),
        }
    }

    pub fn mem_size(&self) -> u32 {
        self.core.arena.len()
    }
}

impl Drop for SkiplistCore {
    fn drop(&mut self) {
        let mut node = self.head.as_ptr();
        loop {
            let next = unsafe { (&*node).next_mut(0) };
            if !next.is_null() {
                unsafe {
                    ptr::drop_in_place(node);
                }
                node = next;
                continue;
            }
            unsafe { ptr::drop_in_place(node) };
            return;
        }
    }
}

unsafe impl<C: Send> Send for Skiplist<C> {}
unsafe impl<C: Sync> Sync for Skiplist<C> {}

pub struct IterRef<'a, C> {
    list: &'a Skiplist<C>,
    cursor: *const Node,
}

impl<'a, C: KeyComparator> IterRef<'a, C> {
    pub fn valid(&self) -> bool {
        !self.cursor.is_null()
    }

    pub fn key(&self) -> &Bytes {
        assert!(self.valid());
        unsafe { &(*self.cursor).key }
    }

    pub fn value(&self) -> &Bytes {
        assert!(self.valid());
        unsafe { &(*self.cursor).value }
    }

    pub fn next(&mut self) {
        assert!(self.valid());
        unsafe {
            self.cursor = (&*self.cursor).next_mut(0);
        }
    }

    pub fn prev(&mut self) {
        assert!(self.valid());
        unsafe {
            self.cursor = self.list.find_near(self.key(), true, false);
        }
    }

    pub fn seek(&mut self, target: &[u8]) {
        unsafe {
            self.cursor = self.list.find_near(target, false, true);
        }
    }

    pub fn seek_for_prev(&mut self, target: &[u8]) {
        unsafe {
            self.cursor = self.list.find_near(target, true, true);
        }
    }

    pub fn seek_to_first(&mut self) {
        unsafe { self.cursor = (&*self.list.core.head.as_ptr()).next_mut(0) }
    }

    pub fn seek_to_last(&mut self) {
        self.cursor = self.list.find_last();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedLengthSuffixComparator;

    #[test]
    fn test_find_near() {
        let comp = FixedLengthSuffixComparator::new(8);
        let list = Skiplist::with_capacity(comp, 1 << 20);
        for i in 0..1000 {
            let key = Bytes::from(format!("{:05}{:08}", i * 10 + 5, 0));
            let value = Bytes::from(format!("{:05}", i));
            list.put(key, value);
        }
        let mut cases = vec![
            ("00001", false, false, Some("00005")),
            ("00001", false, true, Some("00005")),
            ("00001", true, false, None),
            ("00001", true, true, None),
            ("00005", false, false, Some("00015")),
            ("00005", false, true, Some("00005")),
            ("00005", true, false, None),
            ("00005", true, true, Some("00005")),
            ("05555", false, false, Some("05565")),
            ("05555", false, true, Some("05555")),
            ("05555", true, false, Some("05545")),
            ("05555", true, true, Some("05555")),
            ("05558", false, false, Some("05565")),
            ("05558", false, true, Some("05565")),
            ("05558", true, false, Some("05555")),
            ("05558", true, true, Some("05555")),
            ("09995", false, false, None),
            ("09995", false, true, Some("09995")),
            ("09995", true, false, Some("09985")),
            ("09995", true, true, Some("09995")),
            ("59995", false, false, None),
            ("59995", false, true, None),
            ("59995", true, false, Some("09995")),
            ("59995", true, true, Some("09995")),
        ];
        for (i, (key, less, allow_equal, exp)) in cases.drain(..).enumerate() {
            let seek_key = Bytes::from(format!("{}{:08}", key, 0));
            let res = unsafe { list.find_near(&seek_key, less, allow_equal) };
            if exp.is_none() {
                assert!(res.is_null(), "{}", i);
                continue;
            }
            let e = format!("{}{:08}", exp.unwrap(), 0);
            assert_eq!(&unsafe { &*res }.key, e.as_bytes(), "{}", i);
        }
    }
}
