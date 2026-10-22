use std::collections::VecDeque;
use std::net::SocketAddr;

use parking_lot::RwLock;

use super::node::{Node, NodeId};

const K: usize = 8;
const NUM_BUCKETS: usize = 160;

#[derive(Debug)]
struct Bucket {
    nodes: VecDeque<Node>,
    replacement_cache: VecDeque<Node>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            nodes: VecDeque::with_capacity(K),
            replacement_cache: VecDeque::with_capacity(K),
        }
    }

    fn add(&mut self, node: Node) -> bool {
        if let Some(pos) = self.nodes.iter().position(|n| n.id == node.id) {
            let mut existing = self.nodes.remove(pos).unwrap();
            existing.touch();
            self.nodes.push_back(existing);
            return true;
        }

        if self.nodes.len() < K {
            self.nodes.push_back(node);
            return true;
        }

        if self.replacement_cache.len() < K {
            self.replacement_cache.push_back(node);
        }

        false
    }

    fn remove(&mut self, id: &NodeId) -> Option<Node> {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == id) {
            let removed = self.nodes.remove(pos);

            if let Some(replacement) = self.replacement_cache.pop_front() {
                self.nodes.push_back(replacement);
            }

            return removed;
        }

        None
    }

    fn get(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    fn get_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| &n.id == id)
    }

    fn oldest(&self) -> Option<&Node> {
        self.nodes.front()
    }

    fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter()
    }

    fn good_nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.is_good())
    }
}

pub struct RoutingTable {
    our_id: NodeId,
    buckets: Vec<RwLock<Bucket>>,
}

impl RoutingTable {
    pub fn new(our_id: NodeId) -> Self {
        let buckets = (0..NUM_BUCKETS)
            .map(|_| RwLock::new(Bucket::new()))
            .collect();

        Self { our_id, buckets }
    }

    pub fn our_id(&self) -> &NodeId {
        &self.our_id
    }

    pub fn add_node(&self, node: Node) {
        if node.id == self.our_id {
            return;
        }

        let bucket_idx = self.our_id.bucket_index(&node.id);
        let mut bucket = self.buckets[bucket_idx].write();
        bucket.add(node);
    }

    pub fn remove_node(&self, id: &NodeId) {
        let bucket_idx = self.our_id.bucket_index(id);
        let mut bucket = self.buckets[bucket_idx].write();
        bucket.remove(id);
    }

    pub fn mark_failed(&self, id: &NodeId) {
        let bucket_idx = self.our_id.bucket_index(id);
        let mut bucket = self.buckets[bucket_idx].write();

        if let Some(node) = bucket.get_mut(id) {
            node.fail();

            if node.is_bad() {
                bucket.remove(id);
            }
        }
    }

    pub fn mark_seen(&self, id: &NodeId) {
        let bucket_idx = self.our_id.bucket_index(id);
        let mut bucket = self.buckets[bucket_idx].write();

        if let Some(node) = bucket.get_mut(id) {
            node.touch();
        }
    }

    pub fn find_closest(&self, target: &NodeId, count: usize) -> Vec<Node> {
        let mut nodes: Vec<(Node, [u8; 20])> = Vec::new();

        for bucket in &self.buckets {
            let bucket = bucket.read();
            for node in bucket.good_nodes() {
                let dist = node.id.distance(target);
                nodes.push((node.clone(), dist));
            }
        }

        nodes.sort_by(|a, b| a.1.cmp(&b.1));
        nodes.truncate(count);
        nodes.into_iter().map(|(n, _)| n).collect()
    }

    pub fn node_count(&self) -> usize {
        self.buckets.iter().map(|b| b.read().nodes.len()).sum()
    }

    pub fn all_nodes(&self) -> Vec<Node> {
        let mut nodes = Vec::new();
        for bucket in &self.buckets {
            let bucket = bucket.read();
            nodes.extend(bucket.nodes().cloned());
        }
        nodes
    }

    pub fn find_node(&self, id: &NodeId) -> Option<Node> {
        let bucket_idx = self.our_id.bucket_index(id);
        let bucket = self.buckets[bucket_idx].read();
        bucket.get(id).cloned()
    }

    pub fn find_by_addr(&self, addr: &SocketAddr) -> Option<Node> {
        for bucket in &self.buckets {
            let bucket = bucket.read();
            for node in bucket.nodes() {
                if &node.addr == addr {
                    return Some(node.clone());
                }
            }
        }
        None
    }

    pub fn stale_buckets(&self) -> Vec<usize> {
        let mut stale = Vec::new();

        for (i, bucket) in self.buckets.iter().enumerate() {
            let bucket = bucket.read();
            if let Some(oldest) = bucket.oldest() {
                if oldest.last_seen.elapsed().as_secs() > 15 * 60 {
                    stale.push(i);
                }
            }
        }

        stale
    }
}
