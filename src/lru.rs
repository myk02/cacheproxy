//! A byte-capped LRU cache.
//!
//! Entries live in a slab (`Vec<Option<Node>>`) and are linked into a doubly
//! linked list by index, so `get`, `insert` and `remove` are all O(1).
//! The cap is measured in bytes (the size you pass to `insert`), not in the
//! number of entries.

use std::collections::HashMap;
use std::hash::Hash;

struct Node<K, V> {
    key: K,
    value: V,
    size: usize,
    prev: Option<usize>,
    next: Option<usize>,
}

pub struct LruCache<K, V> {
    map: HashMap<K, usize>,
    nodes: Vec<Option<Node<K, V>>>,
    free: Vec<usize>,
    head: Option<usize>, // most recently used
    tail: Option<usize>, // least recently used
    bytes: usize,
    max_bytes: usize,
    evictions: u64,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    pub fn new(max_bytes: usize) -> Self {
        LruCache {
            map: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: None,
            tail: None,
            bytes: 0,
            max_bytes,
            evictions: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bytes currently used.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// How many entries were pushed out to make room.
    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Look up a key and mark it as most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let idx = *self.map.get(key)?;
        self.touch(idx);
        self.nodes[idx].as_ref().map(|n| &n.value)
    }

    /// Look up a key without changing the recency order.
    pub fn peek(&self, key: &K) -> Option<&V> {
        let idx = *self.map.get(key)?;
        self.nodes[idx].as_ref().map(|n| &n.value)
    }

    /// Insert (or replace) an entry of `size` bytes. Least recently used
    /// entries are evicted until it fits. Returns `false`, and stores
    /// nothing, if the entry alone is bigger than the whole cache. Any old
    /// value under the same key is removed first either way.
    pub fn insert(&mut self, key: K, value: V, size: usize) -> bool {
        self.remove(&key);
        if size > self.max_bytes {
            return false;
        }
        while self.bytes + size > self.max_bytes {
            if !self.evict_lru() {
                break;
            }
        }
        let idx = self.alloc(Node {
            key: key.clone(),
            value,
            size,
            prev: None,
            next: None,
        });
        self.map.insert(key, idx);
        self.attach_front(idx);
        self.bytes += size;
        true
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let idx = self.map.remove(key)?;
        Some(self.take_node(idx).value)
    }

    fn evict_lru(&mut self) -> bool {
        let Some(tail) = self.tail else { return false };
        let node = self.take_node(tail);
        self.map.remove(&node.key);
        self.evictions += 1;
        true
    }

    fn alloc(&mut self, node: Node<K, V>) -> usize {
        if let Some(i) = self.free.pop() {
            self.nodes[i] = Some(node);
            i
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        }
    }

    /// Unlink a node, free its slot and give back its bytes.
    fn take_node(&mut self, idx: usize) -> Node<K, V> {
        self.detach(idx);
        let node = self.nodes[idx].take().expect("live node");
        self.free.push(idx);
        self.bytes -= node.size;
        node
    }

    fn detach(&mut self, idx: usize) {
        let (prev, next) = {
            let n = self.nodes[idx].as_ref().expect("live node");
            (n.prev, n.next)
        };
        match prev {
            Some(p) => self.nodes[p].as_mut().expect("live node").next = next,
            None => self.head = next,
        }
        match next {
            Some(n) => self.nodes[n].as_mut().expect("live node").prev = prev,
            None => self.tail = prev,
        }
        let n = self.nodes[idx].as_mut().expect("live node");
        n.prev = None;
        n.next = None;
    }

    fn attach_front(&mut self, idx: usize) {
        let old_head = self.head;
        {
            let n = self.nodes[idx].as_mut().expect("live node");
            n.prev = None;
            n.next = old_head;
        }
        if let Some(h) = old_head {
            self.nodes[h].as_mut().expect("live node").prev = Some(idx);
        }
        self.head = Some(idx);
        if self.tail.is_none() {
            self.tail = Some(idx);
        }
    }

    fn touch(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        self.detach(idx);
        self.attach_front(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max: usize) -> LruCache<String, u32> {
        LruCache::new(max)
    }

    fn k(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn insert_and_get() {
        let mut c = cache(100);
        assert!(c.insert(k("a"), 1, 10));
        assert_eq!(c.get(&k("a")), Some(&1));
        assert_eq!(c.get(&k("b")), None);
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes(), 10);
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut c = cache(30);
        c.insert(k("a"), 1, 10);
        c.insert(k("b"), 2, 10);
        c.insert(k("c"), 3, 10);
        c.insert(k("d"), 4, 10); // forces out "a"
        assert!(!c.contains(&k("a")));
        assert!(c.contains(&k("b")) && c.contains(&k("c")) && c.contains(&k("d")));
        assert_eq!(c.evictions(), 1);
        assert_eq!(c.bytes(), 30);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c = cache(30);
        c.insert(k("a"), 1, 10);
        c.insert(k("b"), 2, 10);
        c.insert(k("c"), 3, 10);
        assert_eq!(c.get(&k("a")), Some(&1)); // "b" is now the oldest
        c.insert(k("d"), 4, 10);
        assert!(c.contains(&k("a")));
        assert!(!c.contains(&k("b")));
    }

    #[test]
    fn peek_does_not_refresh_recency() {
        let mut c = cache(20);
        c.insert(k("a"), 1, 10);
        c.insert(k("b"), 2, 10);
        assert_eq!(c.peek(&k("a")), Some(&1));
        c.insert(k("c"), 3, 10);
        assert!(!c.contains(&k("a")));
    }

    #[test]
    fn one_big_insert_evicts_many() {
        let mut c = cache(30);
        c.insert(k("a"), 1, 10);
        c.insert(k("b"), 2, 10);
        c.insert(k("c"), 3, 10);
        assert!(c.insert(k("big"), 9, 25));
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes(), 25);
        assert_eq!(c.evictions(), 3);
    }

    #[test]
    fn too_big_is_rejected_and_old_value_removed() {
        let mut c = cache(50);
        c.insert(k("a"), 1, 10);
        assert!(!c.insert(k("a"), 2, 51));
        assert!(!c.contains(&k("a")));
        assert_eq!(c.bytes(), 0);
        assert!(c.is_empty());
    }

    #[test]
    fn replacing_a_key_updates_bytes() {
        let mut c = cache(100);
        c.insert(k("a"), 1, 40);
        c.insert(k("a"), 2, 15);
        assert_eq!(c.get(&k("a")), Some(&2));
        assert_eq!(c.bytes(), 15);
        assert_eq!(c.len(), 1);
        assert_eq!(c.evictions(), 0);
    }

    #[test]
    fn remove_frees_bytes_and_slot() {
        let mut c = cache(100);
        c.insert(k("a"), 1, 40);
        c.insert(k("b"), 2, 40);
        assert_eq!(c.remove(&k("a")), Some(1));
        assert_eq!(c.remove(&k("a")), None);
        assert_eq!(c.bytes(), 40);
        assert!(c.insert(k("c"), 3, 60));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn many_operations_keep_invariants() {
        let mut c: LruCache<u32, u32> = LruCache::new(1000);
        for i in 0..5000u32 {
            let size = 10 + (i % 37) as usize;
            c.insert(i % 200, i, size);
            if i % 3 == 0 {
                c.get(&((i / 2) % 200));
            }
            if i % 11 == 0 {
                c.remove(&((i / 3) % 200));
            }
            assert!(c.bytes() <= c.max_bytes());
        }
        // bytes accounting must equal the sum of live nodes
        let sum: usize = c.nodes.iter().flatten().map(|n| n.size).sum();
        assert_eq!(sum, c.bytes());
        assert_eq!(c.nodes.iter().flatten().count(), c.len());
    }
}
