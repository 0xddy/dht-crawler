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

/// 分片节点队列 - 支持高并发，IPv4 和 IPv6 节点分开存储
pub struct ShardedNodeQueue {
    shards_v4: Vec<Mutex<NodeQueueShard>>,  // IPv4 节点分片
    shards_v6: Vec<Mutex<NodeQueueShard>>,  // IPv6 节点分片
}

impl ShardedNodeQueue {
    /// 创建新的分片队列
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
    
    /// 添加节点（根据地址类型自动存入对应队列）
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
    
    /// 批量弹出节点
    /// 
    /// # Arguments
    /// * `count` - 需要获取的节点数量
    /// * `filter_ipv6` - 如果为 `Some(true)`，只从 IPv6 队列获取；如果为 `Some(false)`，只从 IPv4 队列获取；如果为 `None`，从两个队列混合获取
    pub fn pop_batch(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        let mut result = Vec::with_capacity(count);
        let per_shard = (count + QUEUE_SHARD_COUNT - 1) / QUEUE_SHARD_COUNT;
        
        match filter_ipv6 {
            Some(true) => {
                // 只从 IPv6 队列获取
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
                // 只从 IPv4 队列获取
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
                // 混合模式：从两个队列交替获取
                for i in 0..QUEUE_SHARD_COUNT {
                    if result.len() >= count {
                        break;
                    }
                    
                    // 从 IPv4 分片获取
                    let mut s4 = self.shards_v4[i].lock().unwrap();
                    let nodes4 = s4.pop_batch(per_shard / 2);
                    result.extend(nodes4);
                    drop(s4);
                    
                    if result.len() >= count {
                        break;
                    }
                    
                    // 从 IPv6 分片获取
                    let mut s6 = self.shards_v6[i].lock().unwrap();
                    let nodes6 = s6.pop_batch(per_shard / 2);
                    result.extend(nodes6);
                    drop(s6);
                }
            },
        }
        
        result
    }
    
    /// 获取随机节点（用于DHT响应）
    /// 🚀 优化：IPv4 和 IPv6 分开存储，直接从对应队列获取，无需过滤
    /// 
    /// # Arguments
    /// * `count` - 需要获取的节点数量
    /// * `filter_ipv6` - 如果为 `Some(true)`，只返回 IPv6 节点；如果为 `Some(false)`，只返回 IPv4 节点；如果为 `None`，返回所有节点（混合）
    pub fn get_random_nodes(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        match filter_ipv6 {
            Some(true) => {
                // 只要 IPv6 节点
                self.get_random_nodes_from_shards(&self.shards_v6, count)
            },
            Some(false) => {
                // 只要 IPv4 节点
                self.get_random_nodes_from_shards(&self.shards_v4, count)
            },
            None => {
                // 混合模式：从两个队列各取一半
                let count_v4 = count / 2;
                let count_v6 = count - count_v4;
                let mut result = Vec::with_capacity(count);
                
                result.extend(self.get_random_nodes_from_shards(&self.shards_v4, count_v4));
                result.extend(self.get_random_nodes_from_shards(&self.shards_v6, count_v6));
                
                result
            },
        }
    }
    
    /// 从指定的分片组中获取随机节点
    fn get_random_nodes_from_shards(&self, shards: &[Mutex<NodeQueueShard>], count: usize) -> Vec<NodeTuple> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        
        // 🚀 策略1：小规模请求用快速路径（最常见：8个节点）
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
        } else {
            // 🚀 策略2：大规模请求用储层采样
            let mut result = Vec::with_capacity(count);
            let mut seen = 0usize;
            
            // 储层采样算法
            for shard in shards {
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
    }
    
    
    /// 获取总长度（IPv4 + IPv6）
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
    
    /// 检查是否为空
    pub fn is_empty(&self) -> bool {
        let empty_v4 = self.shards_v4
            .iter()
            .all(|shard| shard.lock().unwrap().is_empty());
        let empty_v6 = self.shards_v6
            .iter()
            .all(|shard| shard.lock().unwrap().is_empty());
        empty_v4 && empty_v6
    }
    
    /// 检查指定地址族的队列是否为空
    /// 
    /// # Arguments
    /// * `filter_ipv6` - 如果为 `Some(true)`，检查 IPv6 队列；如果为 `Some(false)`，检查 IPv4 队列；如果为 `None`，检查两个队列
    pub fn is_empty_for(&self, filter_ipv6: Option<bool>) -> bool {
        match filter_ipv6 {
            Some(true) => {
                // 检查 IPv6 队列
                self.shards_v6
                    .iter()
                    .all(|shard| shard.lock().unwrap().is_empty())
            },
            Some(false) => {
                // 检查 IPv4 队列
                self.shards_v4
                    .iter()
                    .all(|shard| shard.lock().unwrap().is_empty())
            },
            None => self.is_empty(),
        }
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

