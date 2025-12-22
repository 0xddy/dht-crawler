use crate::error::Result;
use crate::metadata::RbitFetcher;
use crate::protocol::{DhtMessage, DhtArgs, DhtResponse};
use crate::scheduler::MetadataScheduler;
use crate::types::{DHTOptions, TorrentInfo};
use crate::sharded::{ShardedBloom, ShardedNodeQueue, NodeTuple};
use rand::Rng;
use ahash::AHasher;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Semaphore};
use socket2::{Socket, Domain, Type, Protocol};
use std::pin::Pin;
use std::future::Future;

const BOOTSTRAP_NODES: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.aelitis.com:6881",
];

// 类型定义
pub type BoxedBoolFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
pub type MetadataFetchCallback = Arc<dyn Fn(String) -> BoxedBoolFuture + Send + Sync>;

// Hash 发现事件
/// DHT Server 发现 hash 后发送此事件，由独立的 MetadataScheduler 处理
#[derive(Debug, Clone)]
pub struct HashDiscovered {
    pub info_hash: String,
    pub peer_addr: SocketAddr,
    pub discovered_at: std::time::Instant,
}

// ---------------------------------------------------------------

type TorrentCallback = Arc<dyn Fn(TorrentInfo) + Send + Sync>;
type FilterCallback = Arc<dyn Fn(&str) -> bool + Send + Sync>;
type DuplicateCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct DHTServer {
    #[allow(dead_code)]
    options: DHTOptions,
    node_id: Vec<u8>,
    socket: Arc<UdpSocket>,
    token_secret: Vec<u8>,

    // 这些回调现在与 MetadataScheduler 共享
    callback: Arc<RwLock<Option<TorrentCallback>>>,
    filter: Arc<RwLock<Option<FilterCallback>>>,
    on_duplicate: Arc<RwLock<Option<DuplicateCallback>>>,
    on_metadata_fetch: Arc<RwLock<Option<MetadataFetchCallback>>>,

    // 使用分片锁，大幅减少竞争
    node_queue: Arc<ShardedNodeQueue>,
    seen_hashes: Arc<ShardedBloom>,

    // 发送 hash 发现事件
    hash_tx: mpsc::Sender<HashDiscovered>,
    
    // Metadata 队列长度（用于自适应爬取速度）
    metadata_queue_len: Arc<AtomicUsize>,
    max_metadata_queue_size: usize,
}

impl DHTServer {
    pub async fn new(options: DHTOptions) -> Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        #[cfg(not(windows))]
        { let _ = socket.set_reuse_port(true); }
        let _ = socket.set_reuse_address(true);
        socket.set_nonblocking(true)?;
        
        // 增加网络缓冲区以应对高QPS
        let _ = socket.set_recv_buffer_size(32 * 1024 * 1024);  // 32MB（原16MB）
        let _ = socket.set_send_buffer_size(8 * 1024 * 1024);   // 8MB（原4MB）

        let addr: SocketAddr = format!("0.0.0.0:{}", options.port).parse().unwrap();
        socket.bind(&addr.into())?;
        let socket = UdpSocket::from_std(socket.into())?;

        let node_id = generate_random_id();
        let mut rng = rand::thread_rng();
        let token_secret: Vec<u8> = (0..10).map(|_| rng.gen()).collect();

        // 使用分片队列和分片布隆过滤器
        // 队列容量：100000 个节点（扩容以适应 DHT 网络裂变速度）
        let node_queue = ShardedNodeQueue::new(100000);
        
        // 布隆过滤器：预期500万元素，0.1%误判率
        // 内存使用：约 90MB（32分片 × 2.8MB）
        let bloom = ShardedBloom::new_for_fp_rate(5_000_000, 0.001);

        // -----------------------------------------------------------
        // 内部初始化 MetadataScheduler
        // -----------------------------------------------------------
        let (hash_tx, hash_rx) = mpsc::channel::<HashDiscovered>(10000);

        let fetcher = Arc::new(RbitFetcher::new(options.metadata_timeout));
        
        // 创建共享的回调状态
        let callback = Arc::new(RwLock::new(None));
        let on_metadata_fetch = Arc::new(RwLock::new(None));
        
        // 创建共享的队列长度计数器
        let metadata_queue_len = Arc::new(AtomicUsize::new(0));

        let scheduler = MetadataScheduler::new(
            hash_rx,
            fetcher,
            options.max_metadata_queue_size,
            options.max_metadata_worker_count,
            callback.clone(),
            on_metadata_fetch.clone(),
            metadata_queue_len.clone(),
        );

        // 启动 Scheduler
        tokio::spawn(async move {
            scheduler.run().await;
        });

        let server = Self {
            options: options.clone(),
            node_id: node_id.clone(),
            socket: Arc::new(socket),
            token_secret,
            callback,
            on_metadata_fetch,
            node_queue: Arc::new(node_queue),
            seen_hashes: Arc::new(bloom),
            filter: Arc::new(RwLock::new(None)),
            on_duplicate: Arc::new(RwLock::new(None)),
            hash_tx,
            metadata_queue_len,
            max_metadata_queue_size: options.max_metadata_queue_size,
        };

        Ok(server)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// 设置元数据获取前的检查回调
    ///
    /// 此回调在发现新的 info_hash 后，但在实际连接对等端获取元数据之前执行。
    /// 你可以在这里进行去重检查（如查询数据库），返回 `true` 表示继续获取，`false` 表示跳过。
    ///
    /// # 注意事项
    /// - 回调是在 `MetadataScheduler` 的 Worker 线程中异步执行的（通过 `.await`）。
    /// - 支持耗时操作（如数据库查询），但请注意 Worker 数量限制（默认 500）。
    /// - 如果回调执行过慢，可能会导致任务队列堆积。
    ///
    /// # 示例
    /// ```rust
    /// server.on_metadata_fetch(|hash| async move {
    ///     // 检查数据库是否存在
    ///     // let exists = db.has(hash).await;
    ///     // !exists
    ///     true
    /// });
    /// ```
    pub fn on_metadata_fetch<F, Fut>(&self, callback: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        *self.on_metadata_fetch.write().unwrap() = Some(Arc::new(move |hash| {
            Box::pin(callback(hash))
        }));
    }

    /// 设置成功获取到种子信息的回调
    ///
    /// 当成功从对等端下载并解析出种子元数据（Metadata）后调用。
    ///
    /// # 注意事项
    /// - 此回调是在 Worker 线程中同步执行的。
    /// - 如果包含耗时操作（如写入大量数据或复杂计算），**必须**在回调内部手动使用 `tokio::spawn`。
    /// - 否则会阻塞当前的元数据获取 Worker，降低系统吞吐量。
    ///
    /// # 示例
    /// ```rust
    /// server.on_torrent(|info| {
    ///     // 简单操作可以直接做
    ///     println!("Got torrent: {}", info.name);
    ///     
    ///     // 耗时操作建议 spawn
    ///     tokio::spawn(async move {
    ///         save_to_db(info).await;
    ///     });
    /// });
    /// ```
    pub fn on_torrent<F>(&self, callback: F) where F: Fn(TorrentInfo) + Send + Sync + 'static {
        *self.callback.write().unwrap() = Some(Arc::new(callback));
    }
    
    /// 设置 Hash 过滤器
    ///
    /// 在处理 `announce_peer` 消息时，用于快速判断是否应该处理该 Hash。
    /// 这通常用于布隆过滤器之前的黑名单或白名单机制。
    ///
    /// # 注意事项
    /// - 此回调是在 UDP 处理线程中**同步执行**的。
    /// - **绝对禁止**执行任何耗时操作（如 IO、数据库查询、锁等待）。
    /// - 任何延迟都会直接阻塞网络包的接收，导致丢包。
    /// - 应仅进行纯内存的快速判断。
    pub fn set_filter<F>(&self, filter: F) where F: Fn(&str) -> bool + Send + Sync + 'static {
        *self.filter.write().unwrap() = Some(Arc::new(filter));
    }

    /// 设置重复 Hash 发现的回调
    ///
    /// 当接收到的 Hash 已经被布隆过滤器标记为“已存在”时调用。
    ///
    /// # 注意事项
    /// - 库内部已经自动为每次调用包裹了 `tokio::spawn`。
    /// - 因此你可以放心地在回调中执行耗时操作（如数据库记录），而不用担心阻塞 UDP 线程。
    /// - 虽然内部有 spawn，但频繁触发仍会产生大量任务，请注意资源控制。
    pub fn on_duplicate<F>(&self, callback: F) where F: Fn(&str) + Send + Sync + 'static {
        *self.on_duplicate.write().unwrap() = Some(Arc::new(callback));
    }

    pub fn get_seen_count(&self) -> usize {
        // 分片布隆过滤器的位数统计
        self.seen_hashes.number_of_bits() as usize
    }

    pub fn get_node_pool_size(&self) -> usize {
        self.node_queue.len()
    }

    pub async fn start(&self) -> Result<()> {

        self.start_receiver();
        self.bootstrap().await;

        let server = self.clone();

        tokio::spawn(async move {
            let semaphore = Arc::new(Semaphore::new(2000));
            let mut loop_tick = 0;

            loop {
                // 自适应爬取速度：根据 Metadata 队列负载调整爬取策略
                let queue_len = server.metadata_queue_len.load(Ordering::Relaxed);
                let queue_pressure = queue_len as f64 / server.max_metadata_queue_size as f64;
                
                // 动态计算批次大小和休眠时间
                let (batch_size, sleep_duration) = if queue_pressure < 0.5 {
                    // 🟢 绿区：队列空闲，全速爬取
                    (200, Duration::from_millis(10))
                } else if queue_pressure < 0.8 {
                    // 🟡 黄区：队列有压力，适度减速
                    (200, Duration::from_millis(20))
                } else if queue_pressure < 0.95 {
                    // 🟠 橙区：队列高压，大幅减速
                    (20, Duration::from_millis(500))
                } else {
                    // 🔴 红区：队列爆满，暂停主动爬取
                    (0, Duration::from_millis(1000))
                };

                let nodes_batch = {
                    if server.node_queue.is_empty() || batch_size == 0 {
                        None
                    } else {
                        Some(server.node_queue.pop_batch(batch_size))
                    }
                };

                loop_tick += 1;
                if nodes_batch.is_none() || loop_tick % 50 == 0 {
                    server.bootstrap().await;
                    if nodes_batch.is_none() {
                        tokio::time::sleep(sleep_duration).await;
                        continue;
                    }
                }

                if let Some(nodes) = nodes_batch {
                    for node in nodes {
                        let permit = semaphore.clone().acquire_owned().await.unwrap();
                        let server_clone = server.clone();
                        tokio::spawn(async move {
                            let neighbor_id = generate_neighbor_target(&node.id, &server_clone.node_id);
                            let random_target = generate_random_id();
                            let _ = server_clone.send_find_node(node.addr, &random_target, &neighbor_id).await;
                            drop(permit);
                        });
                    }
                }

                tokio::time::sleep(sleep_duration).await;
            }
        });

        std::future::pending::<()>().await;
        Ok(())
    }

    fn start_receiver(&self) {
        let socket = self.socket.clone();
        let server = self.clone();

        let num_workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);

        let queue_size = 5000;

        let mut senders = Vec::with_capacity(num_workers);
        for _ in 0..num_workers {
            let (tx, mut rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(queue_size);
            senders.push(tx);

            let server_clone = server.clone();

            tokio::spawn(async move {
                while let Some((data, addr)) = rx.recv().await {
                    let _ = server_clone.handle_message(&data, addr).await;
                }
            });
        }

        tokio::spawn(async move {
            let mut buf = [0u8; 65536];
            let mut next_worker_idx = 0;

            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((size, addr)) => {
                        // 🛡️ 安全检查1：拒绝异常大的包（DHT 消息通常 < 2KB）
                        if size > 8192 {
                            #[cfg(debug_assertions)]
                            log::trace!("⚠️ 拒绝异常大的 UDP 包: {} 字节 from {}", size, addr);
                            continue;
                        }
                        
                        // 🛡️ 安全检查2：快速检查是否是有效的 Bencode 字典
                        // DHT KRPC 消息（BEP-5）必须是字典，首字符必须是 'd'
                        if size == 0 || buf[0] != b'd' {
                            continue;
                        }

                        let data = buf[..size].to_vec();

                        let tx = &senders[next_worker_idx];
                        next_worker_idx = (next_worker_idx + 1) % num_workers;

                        match tx.try_send((data, addr)) {
                            Ok(_) => {},
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                #[cfg(debug_assertions)]
                                log::trace!("UDP worker queue full, dropping packet");
                            },
                            Err(_) => { break; }
                        }
                    }
                    Err(_e) => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        });
    }

    async fn handle_message(&self, data: &[u8], addr: SocketAddr) -> Result<()> {
        let msg: DhtMessage = match serde_bencode::from_bytes(data) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };

        match msg.y.as_str() {
            "q" => {
                if let Some(q_type) = &msg.q {
                    self.handle_query(&msg, q_type.as_bytes(), addr).await?;
                }
            }
            "r" => {
                if let Some(response) = &msg.r {
                    self.handle_response(response).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_query(&self, msg: &DhtMessage, query_type: &[u8], addr: SocketAddr) -> Result<()> {
        let args = match &msg.a {
            Some(a) => a,
            None => return Ok(()),
        };

        let transaction_id = &msg.t;
        let sender_id: Option<&[u8]> = args.id.as_deref().map(|v| v.as_slice());
        let target_id_fallback: Option<&[u8]> = args.target.as_deref()
            .or(args.info_hash.as_deref())
            .map(|v| v.as_slice());

        let q_str = std::str::from_utf8(query_type).unwrap_or("");
        
        if q_str == "announce_peer" {
            self.handle_announce_peer(args, addr).await?;
        }

        self.send_response(transaction_id, addr, q_str, sender_id, target_id_fallback).await?;
        Ok(())
    }

    async fn handle_announce_peer(&self, args: &DhtArgs, addr: SocketAddr) -> Result<()> {
        if let Some(token) = &args.token {
            if !self.validate_token(token, addr) { return Ok(()); }
        } else {
            return Ok(());
        }

        if let Some(info_hash) = &args.info_hash {
            let info_hash_arr: [u8; 20] = match info_hash.as_ref().try_into() {
                Ok(arr) => arr, Err(_) => return Ok(()),
            };
            let hash_hex = hex::encode(info_hash_arr);

            // 使用分片布隆过滤器进行高效去重
            let is_duplicate = self.seen_hashes.check_and_set(&info_hash_arr);

            if is_duplicate {
                let dup_cb = self.on_duplicate.read().unwrap().clone();
                if let Some(cb) = dup_cb {
                    let hash_hex_clone = hash_hex.clone();
                    tokio::spawn(async move {
                        cb(&hash_hex_clone);
                    });
                }
                return Ok(());
            }

            let filter_cb = self.filter.read().unwrap().clone();
            if let Some(f) = filter_cb {
                if !f(&hash_hex) { return Ok(()); }
            }

            #[cfg(debug_assertions)]
            log::debug!("🔥 新 Hash: {} 来自 {}", hash_hex, addr);

            // 解耦：发送 hash 发现事件
            let port = if let Some(implied) = args.implied_port {
                if implied != 0 { addr.port() } else { args.port.unwrap_or(0) }
            } else {
                args.port.unwrap_or(addr.port())
            };

            if port > 0 {
                let event = HashDiscovered {
                    info_hash: hash_hex,
                    peer_addr: SocketAddr::new(addr.ip(), port),
                    discovered_at: std::time::Instant::now(),
                };

                // 使用 try_send，队列满时直接丢弃（背压）
                if let Err(_) = self.hash_tx.try_send(event) {
                    #[cfg(debug_assertions)]
                    log::trace!("⚠️ Hash 队列满，丢弃 hash");
                }
            }
        }
        Ok(())
    }

    async fn handle_response(&self, response: &DhtResponse) -> Result<()> {
        if let Some(nodes_bytes) = &response.nodes {
            self.process_compact_nodes(nodes_bytes);
        }
        Ok(())
    }

    fn process_compact_nodes(&self, nodes_bytes: &[u8]) {
        if nodes_bytes.len() % 26 != 0 { return; }

        // 使用分片队列，直接并发插入（无锁竞争）
        for chunk in nodes_bytes.chunks(26) {
            let id = chunk[0..20].to_vec();
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            
            let ip = std::net::Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
            let addr = SocketAddr::new(std::net::IpAddr::V4(ip), port);
            
            self.node_queue.push(NodeTuple { id, addr });
        }
    }

    async fn send_response(
        &self,
        tid: &[u8],
        addr: SocketAddr,
        query_type: &str,
        sender_id: Option<&[u8]>,
        target_id_fallback: Option<&[u8]>,
    ) -> Result<()> {
        let mut r_dict = std::collections::HashMap::new();

        let reference_id = sender_id.or(target_id_fallback);
        let my_id = if let Some(target) = reference_id {
            generate_neighbor_target(target, &self.node_id)
        } else {
            self.node_id.clone()
        };

        r_dict.insert(b"id".to_vec(), serde_bencode::value::Value::Bytes(my_id));
        let token = self.generate_token(addr);
        r_dict.insert(b"token".to_vec(), serde_bencode::value::Value::Bytes(token));

        if query_type == "get_peers" || query_type == "find_node" {
            // 使用分片队列获取随机节点（无锁竞争）
            let nodes = self.node_queue.get_random_nodes(8);
            
            let mut nodes_data = Vec::new();
            for node in nodes {
                nodes_data.extend_from_slice(&node.id);
                match node.addr.ip() {
                    IpAddr::V4(ip) => nodes_data.extend_from_slice(&ip.octets()),
                    _ => continue,
                }
                nodes_data.extend_from_slice(&node.addr.port().to_be_bytes());
            }
            
            r_dict.insert(b"nodes".to_vec(), serde_bencode::value::Value::Bytes(nodes_data));
        }

        let mut response: std::collections::HashMap<String, serde_bencode::value::Value> = std::collections::HashMap::new();
        response.insert("t".to_string(), serde_bencode::value::Value::Bytes(tid.to_vec()));
        response.insert("y".to_string(), serde_bencode::value::Value::Bytes(b"r".to_vec()));
        response.insert("r".to_string(), serde_bencode::value::Value::Dict(r_dict));

        if let Ok(encoded) = serde_bencode::to_bytes(&response) {
            let _ = self.socket.send_to(&encoded, addr).await;
        }
        Ok(())
    }

    async fn bootstrap(&self) {
        let target = generate_random_id();
        for node in BOOTSTRAP_NODES {
            match tokio::net::lookup_host(node).await {
                Ok(addrs) => {
                    for addr in addrs {
                        if addr.is_ipv6() { continue; }
                        let _ = self.send_find_node(addr, &target, &self.node_id).await;
                    }
                }
                Err(_) => {}
            }
        }
    }

    async fn send_find_node(&self, addr: SocketAddr, target: &[u8], sender_id: &[u8]) -> Result<()> {
        let mut args = std::collections::HashMap::new();
        args.insert(b"id".to_vec(), serde_bencode::value::Value::Bytes(sender_id.to_vec()));
        args.insert(b"target".to_vec(), serde_bencode::value::Value::Bytes(target.to_vec()));

        let mut msg: std::collections::HashMap<String, serde_bencode::value::Value> = std::collections::HashMap::new();
        msg.insert("t".to_string(), serde_bencode::value::Value::Bytes(vec![0, 1]));
        msg.insert("y".to_string(), serde_bencode::value::Value::Bytes(b"q".to_vec()));
        msg.insert("q".to_string(), serde_bencode::value::Value::Bytes(b"find_node".to_vec()));
        msg.insert("a".to_string(), serde_bencode::value::Value::Dict(args));

        if let Ok(encoded) = serde_bencode::to_bytes(&msg) {
            let _ = self.socket.send_to(&encoded, addr).await;
        }
        Ok(())
    }

    fn generate_token(&self, addr: SocketAddr) -> Vec<u8> {

        let mut hasher = AHasher::default();
        
        // Hash IP地址
        match addr.ip() {
            IpAddr::V4(ip) => ip.octets().hash(&mut hasher),
            IpAddr::V6(ip) => ip.octets().hash(&mut hasher),
        }
        
        // Hash 密钥
        self.token_secret.hash(&mut hasher);
        
        // 返回 8 字节 token
        let hash = hasher.finish();
        hash.to_le_bytes().to_vec()
    }
    
    fn validate_token(&self, token: &[u8], addr: SocketAddr) -> bool {
        if token.len() != 8 {
            return false;
        }
        let expected = self.generate_token(addr);
        token == expected.as_slice()
    }
}

fn generate_random_id() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    (0..20).map(|_| rng.gen()).collect()
}

fn generate_neighbor_target(remote_id: &[u8], local_id: &[u8]) -> Vec<u8> {
    let mut id = Vec::with_capacity(20);
    let prefix_len = std::cmp::min(remote_id.len(), 6);
    id.extend_from_slice(&remote_id[..prefix_len]);
    if local_id.len() > prefix_len {
        id.extend_from_slice(&local_id[prefix_len..]);
    } else {
        while id.len() < 20 {
            id.push(rand::random());
        }
    }
    id
}
