use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::RwLock;

type CacheKey = (String, u32);

struct CachedPiece {
    data: Bytes,
    #[allow(dead_code)]
    verified: bool,
}

pub struct PieceCache {
    t1: RwLock<LruList>,
    t2: RwLock<LruList>,
    b1: RwLock<GhostList>,
    b2: RwLock<GhostList>,
    p: AtomicUsize,
    capacity: usize,
    memory_used: AtomicUsize,
}

struct LruList {
    order: VecDeque<CacheKey>,
    data: HashMap<CacheKey, CachedPiece>,
}

impl LruList {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            data: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn contains(&self, key: &CacheKey) -> bool {
        self.data.contains_key(key)
    }

    fn get(&self, key: &CacheKey) -> Option<&CachedPiece> {
        self.data.get(key)
    }

    fn insert(&mut self, key: CacheKey, piece: CachedPiece) -> (usize, usize) {
        let new_size = piece.data.len();
        let old_size = if let Some(old_piece) = self.data.insert(key.clone(), piece) {
            old_piece.data.len()
        } else {
            self.order.push_back(key);
            0
        };
        (new_size, old_size)
    }

    fn remove(&mut self, key: &CacheKey) -> Option<(CachedPiece, usize)> {
        if let Some(piece) = self.data.remove(key) {
            self.order.retain(|k| k != key);
            let size = piece.data.len();
            Some((piece, size))
        } else {
            None
        }
    }

    fn pop_front(&mut self) -> Option<(CacheKey, CachedPiece, usize)> {
        while let Some(key) = self.order.pop_front() {
            if let Some(piece) = self.data.remove(&key) {
                let size = piece.data.len();
                return Some((key, piece, size));
            }
        }
        None
    }

    fn move_to_back(&mut self, key: &CacheKey) {
        if self.data.contains_key(key) {
            self.order.retain(|k| k != key);
            self.order.push_back(key.clone());
        }
    }
}

struct GhostList {
    keys: VecDeque<CacheKey>,
    set: HashSet<CacheKey>,
}

impl GhostList {
    fn new() -> Self {
        Self {
            keys: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.set.len()
    }

    fn contains(&self, key: &CacheKey) -> bool {
        self.set.contains(key)
    }

    fn insert(&mut self, key: CacheKey) {
        if self.set.insert(key.clone()) {
            self.keys.push_back(key);
        }
    }

    fn remove(&mut self, key: &CacheKey) -> bool {
        if self.set.remove(key) {
            self.keys.retain(|k| k != key);
            true
        } else {
            false
        }
    }

    fn pop_front(&mut self) -> Option<CacheKey> {
        while let Some(key) = self.keys.pop_front() {
            if self.set.remove(&key) {
                return Some(key);
            }
        }
        None
    }
}

impl PieceCache {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            t1: RwLock::new(LruList::new()),
            t2: RwLock::new(LruList::new()),
            b1: RwLock::new(GhostList::new()),
            b2: RwLock::new(GhostList::new()),
            p: AtomicUsize::new(0),
            capacity,
            memory_used: AtomicUsize::new(0),
        })
    }

    pub fn get(&self, info_hash: &str, piece_index: u32) -> Option<Bytes> {
        let key = (info_hash.to_string(), piece_index);

        {
            let t1 = self.t1.read();
            if let Some(piece) = t1.get(&key) {
                let data = piece.data.clone();
                drop(t1);
                self.promote_t1_to_t2(&key);
                return Some(data);
            }
        }

        {
            let t2 = self.t2.read();
            if let Some(piece) = t2.get(&key) {
                let data = piece.data.clone();
                drop(t2);
                self.t2.write().move_to_back(&key);
                return Some(data);
            }
        }

        None
    }

    fn promote_t1_to_t2(&self, key: &CacheKey) {
        let mut t1 = self.t1.write();
        if let Some((piece, _)) = t1.remove(key) {
            drop(t1);
            let mut t2 = self.t2.write();
            let _ = t2.insert(key.clone(), piece);
        }
    }

    pub fn insert(&self, info_hash: &str, piece_index: u32, data: Bytes, verified: bool) {
        let key = (info_hash.to_string(), piece_index);
        let piece = CachedPiece { data, verified };

        if self.b1.read().contains(&key) {
            self.b1.write().remove(&key);
            let delta = self.compute_delta_b1();
            self.adapt_p(delta as isize);
            self.replace(&key, false);
            let (new_size, old_size) = self.t2.write().insert(key, piece);
            self.update_memory(new_size, old_size);
            return;
        }

        if self.b2.read().contains(&key) {
            self.b2.write().remove(&key);
            let delta = self.compute_delta_b2();
            self.adapt_p(-(delta as isize));
            self.replace(&key, true);
            let (new_size, old_size) = self.t2.write().insert(key, piece);
            self.update_memory(new_size, old_size);
            return;
        }

        let t1_len = self.t1.read().len();
        let b1_len = self.b1.read().len();
        if t1_len + b1_len >= self.capacity {
            if t1_len < self.capacity {
                self.b1.write().pop_front();
                self.replace(&key, false);
            } else if let Some((_, _, size)) = self.t1.write().pop_front() {
                self.memory_used.fetch_sub(size, Ordering::Relaxed);
            }
        } else {
            let total = t1_len + b1_len + self.t2.read().len() + self.b2.read().len();
            if total >= self.capacity {
                if total >= 2 * self.capacity {
                    self.b2.write().pop_front();
                }
                self.replace(&key, false);
            }
        }

        let (new_size, old_size) = self.t1.write().insert(key, piece);
        self.update_memory(new_size, old_size);
    }

    fn update_memory(&self, new_size: usize, old_size: usize) {
        if new_size > old_size {
            self.memory_used
                .fetch_add(new_size - old_size, Ordering::Relaxed);
        } else {
            self.memory_used
                .fetch_sub(old_size - new_size, Ordering::Relaxed);
        }
    }

    fn replace(&self, _key: &CacheKey, in_b2: bool) {
        let t1_len = self.t1.read().len();
        let p = self.p.load(Ordering::Relaxed);

        let evict_from_t1 = t1_len > 0
            && ((in_b2 && t1_len == p) || (!in_b2 && t1_len > p) || self.t2.read().len() == 0);

        if evict_from_t1 {
            if let Some((evicted_key, _, size)) = self.t1.write().pop_front() {
                self.memory_used.fetch_sub(size, Ordering::Relaxed);
                self.b1.write().insert(evicted_key);
            }
        } else if let Some((evicted_key, _, size)) = self.t2.write().pop_front() {
            self.memory_used.fetch_sub(size, Ordering::Relaxed);
            self.b2.write().insert(evicted_key);
        }
    }

    fn compute_delta_b1(&self) -> usize {
        let b1_len = self.b1.read().len();
        let b2_len = self.b2.read().len();
        if b1_len >= b2_len {
            1
        } else {
            b2_len / b1_len.max(1)
        }
    }

    fn compute_delta_b2(&self) -> usize {
        let b1_len = self.b1.read().len();
        let b2_len = self.b2.read().len();
        if b2_len >= b1_len {
            1
        } else {
            b1_len / b2_len.max(1)
        }
    }

    fn adapt_p(&self, delta: isize) {
        let current = self.p.load(Ordering::Relaxed) as isize;
        let new_p = (current + delta).max(0).min(self.capacity as isize) as usize;
        self.p.store(new_p, Ordering::Relaxed);
    }

    pub fn remove(&self, info_hash: &str, piece_index: u32) -> Option<Bytes> {
        let key = (info_hash.to_string(), piece_index);

        if let Some((piece, size)) = self.t1.write().remove(&key) {
            self.memory_used.fetch_sub(size, Ordering::Relaxed);
            return Some(piece.data);
        }

        if let Some((piece, size)) = self.t2.write().remove(&key) {
            self.memory_used.fetch_sub(size, Ordering::Relaxed);
            return Some(piece.data);
        }

        None
    }

    pub fn contains(&self, info_hash: &str, piece_index: u32) -> bool {
        let key = (info_hash.to_string(), piece_index);
        self.t1.read().contains(&key) || self.t2.read().contains(&key)
    }

    pub fn memory_used(&self) -> usize {
        self.memory_used.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.t1.read().len() + self.t2.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.t1.write().data.clear();
        self.t1.write().order.clear();
        self.t2.write().data.clear();
        self.t2.write().order.clear();
        self.b1.write().keys.clear();
        self.b1.write().set.clear();
        self.b2.write().keys.clear();
        self.b2.write().set.clear();
        self.p.store(0, Ordering::Relaxed);
        self.memory_used.store(0, Ordering::Relaxed);
    }
}
