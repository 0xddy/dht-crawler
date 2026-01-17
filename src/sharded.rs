use bloomfilter::Bloom;
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const BLOOM_SHARD_COUNT: usize = 32;
const QUEUE_SHARD_COUNT: usize = 16;

pub struct ShardedBloom {
    shards: Vec<Mutex<Bloom<[u8; 20]>>>,
    count: AtomicUsize,
}

impl ShardedBloom {
    pub fn new_for_fp_rate(expected_items: usize, fp_rate: f64) -> Self {
        let items_per_shard = (expected_items + BLOOM_SHARD_COUNT - 1) / BLOOM_SHARD_COUNT;
        
        let shards = (0..BLOOM_SHARD_COUNT)
            .map(|_| Mutex::new(Bloom::new_for_fp_rate(items_per_shard, fp_rate)))
            .collect();
        
        Self { 
            shards,
            count: AtomicUsize::new(0),
        }
    }
    
    pub fn check_and_set(&self, hash: &[u8; 20]) -> bool {
        let shard_idx = self.hash_to_shard(hash);
        let mut shard = self.shards[shard_idx].lock().unwrap();
        let present = shard.check_and_set(hash);
        
        if !present {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        present
    }
    
    pub fn number_of_bits(&self) -> u64 {
        self.count.load(Ordering::Relaxed) as u64
    }
    
    #[inline]
    fn hash_to_shard(&self, hash: &[u8; 20]) -> usize {
        let idx = (hash[0] as usize) | ((hash[1] as usize) << 8);
        idx % BLOOM_SHARD_COUNT
    }
}

#[derive(Debug, Clone)]
pub struct NodeTuple {
    pub id: Vec<u8>,
    pub addr: SocketAddr,
}

struct NodeQueueShard {
    queue: VecDeque<NodeTuple>,
    index: HashSet<SocketAddr>,
    capacity: usize,
}

impl NodeQueueShard {
    fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            index: HashSet::with_capacity(capacity),
            capacity,
        }
    }
    
    fn push(&mut self, node: NodeTuple) {
        if self.index.contains(&node.addr) {
            return;
        }

        if self.queue.len() >= self.capacity {
            if let Some(removed) = self.queue.pop_front() {
                self.index.remove(&removed.addr);
            }
        }

        self.index.insert(node.addr);
        self.queue.push_back(node);
    }
    
    fn pop_batch(&mut self, count: usize) -> Vec<NodeTuple> {
        let actual_count = count.min(self.queue.len());
        let mut nodes = Vec::with_capacity(actual_count);
        
        for _ in 0..actual_count {
            if let Some(node) = self.queue.pop_front() {
                self.index.remove(&node.addr);
                nodes.push(node);
            }
        }
        nodes
    }
    
    fn len(&self) -> usize {
        self.queue.len()
    }
    
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

pub struct ShardedNodeQueue {
    shards_v4: Vec<Mutex<NodeQueueShard>>,
    shards_v6: Vec<Mutex<NodeQueueShard>>,
}

impl ShardedNodeQueue {
    pub fn new(total_capacity: usize) -> Self {
        let capacity_per_shard = (total_capacity + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        let shards_v4 = (0..QUEUE_SHARD_COUNT)
            .map(|_| Mutex::new(NodeQueueShard::new(capacity_per_shard)))
            .collect();
        
        let shards_v6 = (0..QUEUE_SHARD_COUNT)
            .map(|_| Mutex::new(NodeQueueShard::new(capacity_per_shard)))
            .collect();
        
        Self { shards_v4, shards_v6 }
    }
    
    pub fn push(&self, node: NodeTuple) {
        let shard_idx = self.addr_to_shard(&node.addr);
        
        if node.addr.is_ipv6() {
            let mut shard = self.shards_v6[shard_idx].lock().unwrap();
            shard.push(node);
        } else {
            let mut shard = self.shards_v4[shard_idx].lock().unwrap();
            shard.push(node);
        }
    }
    
    pub fn pop_batch(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        let mut result = Vec::with_capacity(count);
        let per_shard = (count + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        match filter_ipv6 {
            Some(true) => {
                for shard in &self.shards_v6 {
                    if result.len() >= count {
                        break;
                    }
                    let mut s = shard.lock().unwrap();
                    let nodes = s.pop_batch(per_shard);
                    result.extend(nodes);
                }
            },
            Some(false) => {
                for shard in &self.shards_v4 {
                    if result.len() >= count {
                        break;
                    }
                    let mut s = shard.lock().unwrap();
                    let nodes = s.pop_batch(per_shard);
                    result.extend(nodes);
                }
            },
            None => {
                for i in 0..QUEUE_SHARD_COUNT {
                    if result.len() >= count {
                        break;
                    }
                    
                    let mut s4 = self.shards_v4[i].lock().unwrap();
                    let nodes4 = s4.pop_batch(per_shard / 2);
                    result.extend(nodes4);
                    drop(s4);
                    
                    if result.len() >= count {
                        break;
                    }
                    
                    let mut s6 = self.shards_v6[i].lock().unwrap();
                    let nodes6 = s6.pop_batch(per_shard / 2);
                    result.extend(nodes6);
                    drop(s6);
                }
            },
        }
        
        result
    }
    
    pub fn get_random_nodes(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        match filter_ipv6 {
            Some(true) => {
                self.get_random_nodes_from_shards(&self.shards_v6, count)
            },
            Some(false) => {
                self.get_random_nodes_from_shards(&self.shards_v4, count)
            },
            None => {
                let count_v4 = count / 2;
                let count_v6 = count - count_v4;
                let mut result = Vec::with_capacity(count);
                
                result.extend(self.get_random_nodes_from_shards(&self.shards_v4, count_v4));
                result.extend(self.get_random_nodes_from_shards(&self.shards_v6, count_v6));
                
                result
            },
        }
    }
    
    fn get_random_nodes_from_shards(&self, shards: &[Mutex<NodeQueueShard>], count: usize) -> Vec<NodeTuple> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        
        if count <= 16 {
            let mut result = Vec::with_capacity(count);
            let per_shard = (count + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
            
            for shard in shards {
                if result.len() >= count {
                    break;
                }
                
                let s = shard.lock().unwrap();
                let shard_len = s.queue.len();
                
                if shard_len == 0 {
                    continue;
                }
                
                let to_take = per_shard.min(shard_len).min(count - result.len());
                
                let mut indices: Vec<usize> = (0..shard_len).collect();
                
                for i in 0..to_take {
                    let j = rng.gen_range(i..shard_len);
                    indices.swap(i, j);
                }
                
                for i in 0..to_take {
                    if let Some(node) = s.queue.get(indices[i]) {
                        result.push(node.clone());
                    }
                }
            }
            
            result
        } else {
            let mut result = Vec::with_capacity(count);
            let mut seen = 0usize;
            
            for shard in shards {
                let s = shard.lock().unwrap();
                
                for node in s.queue.iter() {
                    seen += 1;
                    
                    if result.len() < count {
                        result.push(node.clone());
                    } else {
                        let j = rng.gen_range(0..seen);
                        if j < count {
                            result[j] = node.clone();
                        }
                    }
                }
            }
            
            result
        }
    }
    
    pub fn len(&self) -> usize {
        let len_v4: usize = self.shards_v4
            .iter()
            .map(|shard| shard.lock().unwrap().len())
            .sum();
        let len_v6: usize = self.shards_v6
            .iter()
            .map(|shard| shard.lock().unwrap().len())
            .sum();
        len_v4 + len_v6
    }
    
    pub fn is_empty(&self) -> bool {
        let empty_v4 = self.shards_v4
            .iter()
            .all(|shard| shard.lock().unwrap().is_empty());
        let empty_v6 = self.shards_v6
            .iter()
            .all(|shard| shard.lock().unwrap().is_empty());
        empty_v4 && empty_v6
    }
    
    pub fn is_empty_for(&self, filter_ipv6: Option<bool>) -> bool {
        match filter_ipv6 {
            Some(true) => {
                self.shards_v6
                    .iter()
                    .all(|shard| shard.lock().unwrap().is_empty())
            },
            Some(false) => {
                self.shards_v4
                    .iter()
                    .all(|shard| shard.lock().unwrap().is_empty())
            },
            None => self.is_empty(),
        }
    }
    
    #[inline]
    fn addr_to_shard(&self, addr: &SocketAddr) -> usize {
        let hash = match addr.ip() {
            std::net::IpAddr::V4(ip) => {
                let octets = ip.octets();
                (octets[3] as usize) ^ (addr.port() as usize)
            }
            std::net::IpAddr::V6(ip) => {
                let octets = ip.octets();
                (octets[15] as usize) ^ (addr.port() as usize)
            }
        };
        hash % QUEUE_SHARD_COUNT
    }
}

