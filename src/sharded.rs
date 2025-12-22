// 分片锁实现 - 大幅减少锁竞争，提升并发性能
//
// 核心思想：1个大锁 → N个小锁
// 性能提升：预期 3-4 倍

use bloomfilter::Bloom;
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// 配置：分片数量
const BLOOM_SHARD_COUNT: usize = 32;  // 32个布隆过滤器分片
const QUEUE_SHARD_COUNT: usize = 16;  // 16个队列分片

// ==================== 分片布隆过滤器 ====================

/// 分片布隆过滤器 - 减少锁竞争
/// 
/// 将单个布隆过滤器拆分为32个分片，每个分片独立锁
/// 不同的hash会落到不同的分片上，大幅减少竞争
pub struct ShardedBloom {
    shards: Vec<Mutex<Bloom<[u8; 20]>>>,
    count: AtomicUsize,
}

impl ShardedBloom {
    /// 创建新的分片布隆过滤器
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
    
    /// 检查并设置元素（原子操作）
    pub fn check_and_set(&self, hash: &[u8; 20]) -> bool {
        let shard_idx = self.hash_to_shard(hash);
        let mut shard = self.shards[shard_idx].lock().unwrap();
        let present = shard.check_and_set(hash);
        
        // 如果之前不存在，增加计数
        if !present {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        present
    }
    
    /// 获取实际发现的唯一 InfoHash 数量
    pub fn number_of_bits(&self) -> u64 {
        self.count.load(Ordering::Relaxed) as u64
    }
    
    /// 根据hash计算分片索引
    #[inline]
    fn hash_to_shard(&self, hash: &[u8; 20]) -> usize {
        // 使用hash的前两个字节计算分片
        let idx = (hash[0] as usize) | ((hash[1] as usize) << 8);
        idx % BLOOM_SHARD_COUNT
    }
}

// ==================== 分片节点队列 ====================

/// 节点信息
#[derive(Debug, Clone)]
pub struct NodeTuple {
    pub id: Vec<u8>,
    pub addr: SocketAddr,
}

/// 单个队列分片
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

        // 如果满了，移除最早的一个（保持流动性，优胜劣汰）
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

/// 分片节点队列 - 支持高并发
pub struct ShardedNodeQueue {
    shards: Vec<Mutex<NodeQueueShard>>,
}

impl ShardedNodeQueue {
    /// 创建新的分片队列
    pub fn new(total_capacity: usize) -> Self {
        let capacity_per_shard = (total_capacity + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        let shards = (0..QUEUE_SHARD_COUNT)
            .map(|_| Mutex::new(NodeQueueShard::new(capacity_per_shard)))
            .collect();
        
        Self { shards }
    }
    
    /// 添加节点
    pub fn push(&self, node: NodeTuple) {
        let shard_idx = self.addr_to_shard(&node.addr);
        let mut shard = self.shards[shard_idx].lock().unwrap();
        shard.push(node);
    }
    
    /// 批量弹出节点
    pub fn pop_batch(&self, count: usize) -> Vec<NodeTuple> {
        let mut result = Vec::with_capacity(count);
        let per_shard = (count + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        // 从所有分片获取
        for shard in &self.shards {
            if result.len() >= count {
                break;
            }
            
            let mut s = shard.lock().unwrap();
            let nodes = s.pop_batch(per_shard);
            result.extend(nodes);
        }
        
        result
    }
    
    /// 获取随机节点（用于DHT响应）
    /// 🚀 优化：使用储层采样算法，O(n)时间，无需clone全部节点
    pub fn get_random_nodes(&self, count: usize) -> Vec<NodeTuple> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        
        // 🚀 策略1：小规模请求用快速路径（最常见：8个节点）
        if count <= 16 {
            return self.get_random_nodes_fast(count);
        }
        
        // 🚀 策略2：大规模请求用储层采样
        let mut result = Vec::with_capacity(count);
        let mut seen = 0usize;
        
        // 储层采样算法
        for shard in &self.shards {
            let s = shard.lock().unwrap();
            
            for node in s.queue.iter() {
                seen += 1;
                
                if result.len() < count {
                    // 前 count 个直接加入
                    result.push(node.clone());
                } else {
                    // 后续以 count/seen 的概率替换
                    let j = rng.gen_range(0..seen);
                    if j < count {
                        result[j] = node.clone();
                    }
                }
            }
        }
        
        result
    }
    
    /// 快速路径：小规模随机选择（针对常见的8节点请求）
    fn get_random_nodes_fast(&self, count: usize) -> Vec<NodeTuple> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut result = Vec::with_capacity(count);
        
        // 从每个分片随机选择几个节点
        let per_shard = (count + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        for shard in &self.shards {
            if result.len() >= count {
                break;
            }
            
            let s = shard.lock().unwrap();
            let shard_len = s.queue.len();
            
            if shard_len == 0 {
                continue;
            }
            
            // 从当前分片随机选择最多 per_shard 个节点
            let to_take = per_shard.min(shard_len).min(count - result.len());
            
            // 生成随机索引（不重复）
            let mut indices: Vec<usize> = (0..shard_len).collect();
            
            // 只 shuffle 前 to_take 个（部分 shuffle，Fisher-Yates 优化）
            for i in 0..to_take {
                let j = rng.gen_range(i..shard_len);
                indices.swap(i, j);
            }
            
            // 取前 to_take 个索引对应的节点
            for i in 0..to_take {
                if let Some(node) = s.queue.get(indices[i]) {
                    result.push(node.clone());
                }
            }
        }
        
        result
    }
    
    /// 获取总长度
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().unwrap().len())
            .sum()
    }
    
    /// 检查是否为空
    pub fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|shard| shard.lock().unwrap().is_empty())
    }
    
    /// 根据地址计算分片索引
    #[inline]
    fn addr_to_shard(&self, addr: &SocketAddr) -> usize {
        // 使用端口和IP最后一个字节
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

