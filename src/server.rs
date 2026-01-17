use crate::error::Result;
use crate::metadata::RbitFetcher;
use crate::protocol::{DhtMessage, DhtArgs, DhtResponse};
use crate::scheduler::MetadataScheduler;
use crate::types::{DHTOptions, TorrentInfo, NetMode};
use crate::sharded::{ShardedNodeQueue, NodeTuple};
use rand::Rng;
use ahash::AHasher;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
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
#[derive(Debug, Clone)]
pub struct HashDiscovered {
    pub info_hash: String,
    pub peer_addr: SocketAddr,
    pub discovered_at: std::time::Instant,
}

// ---------------------------------------------------------------

type TorrentCallback = Arc<dyn Fn(TorrentInfo) + Send + Sync>;
type FilterCallback = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct DHTServer {
    #[allow(dead_code)]
    options: DHTOptions,
    node_id: Vec<u8>,
    socket: Arc<UdpSocket>,
    socket_v6: Option<Arc<UdpSocket>>,
    token_secret: Vec<u8>,

    callback: Arc<RwLock<Option<TorrentCallback>>>,
    filter: Arc<RwLock<Option<FilterCallback>>>,
    on_metadata_fetch: Arc<RwLock<Option<MetadataFetchCallback>>>,

    // 使用分片锁，大幅减少竞争
    node_queue: Arc<ShardedNodeQueue>,

    // 发送 hash 发现事件
    hash_tx: mpsc::Sender<HashDiscovered>,
    
    // Metadata 队列长度（用于自适应爬取速度）
    metadata_queue_len: Arc<AtomicUsize>,
    max_metadata_queue_size: usize,
}

impl DHTServer {
    pub async fn new(options: DHTOptions) -> Result<Self> {
        let (socket, socket_v6) = match options.netmode {
            NetMode::Ipv4Only => {
                let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
                #[cfg(not(windows))]
                { let _ = sock.set_reuse_port(true); }
                let _ = sock.set_reuse_address(true);
                sock.set_nonblocking(true)?;
                
                // 增加网络缓冲区以应对高QPS
                let _ = sock.set_recv_buffer_size(32 * 1024 * 1024);  // 32MB
                let _ = sock.set_send_buffer_size(8 * 1024 * 1024);   // 8MB

                let addr: SocketAddr = format!("0.0.0.0:{}", options.port).parse().unwrap();
                sock.bind(&addr.into())?;
                (Arc::new(UdpSocket::from_std(sock.into())?), None)
            },
            NetMode::Ipv6Only => {
                let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                #[cfg(not(windows))]
                { let _ = sock.set_reuse_port(true); }
                let _ = sock.set_reuse_address(true);
                // 设置仅IPv6模式（Windows默认是仅IPv6，Linux/Unix需要设置）
                #[cfg(not(windows))]
                { let _ = sock.set_only_v6(true); }
                sock.set_nonblocking(true)?;
                
                let _ = sock.set_recv_buffer_size(32 * 1024 * 1024);
                let _ = sock.set_send_buffer_size(8 * 1024 * 1024);

                let addr: SocketAddr = format!("[::]:{}", options.port).parse().unwrap();
                sock.bind(&addr.into())?;
                (Arc::new(UdpSocket::from_std(sock.into())?), None)
            },
            NetMode::DualStack => {
                // IPv4 socket
                let sock_v4 = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
                #[cfg(not(windows))]
                { let _ = sock_v4.set_reuse_port(true); }
                let _ = sock_v4.set_reuse_address(true);
                sock_v4.set_nonblocking(true)?;
                let _ = sock_v4.set_recv_buffer_size(32 * 1024 * 1024);
                let _ = sock_v4.set_send_buffer_size(8 * 1024 * 1024);
                let addr_v4: SocketAddr = format!("0.0.0.0:{}", options.port).parse().unwrap();
                sock_v4.bind(&addr_v4.into())?;
                let socket = Arc::new(UdpSocket::from_std(sock_v4.into())?);

                // IPv6 socket
                let sock_v6 = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                #[cfg(not(windows))]
                { let _ = sock_v6.set_reuse_port(true); }
                let _ = sock_v6.set_reuse_address(true);
                #[cfg(not(windows))]
                { let _ = sock_v6.set_only_v6(true); }  // 仅IPv6，避免与IPv4冲突
                sock_v6.set_nonblocking(true)?;
                let _ = sock_v6.set_recv_buffer_size(32 * 1024 * 1024);
                let _ = sock_v6.set_send_buffer_size(8 * 1024 * 1024);
                let addr_v6: SocketAddr = format!("[::]:{}", options.port).parse().unwrap();
                sock_v6.bind(&addr_v6.into())?;
                let socket_v6 = Some(Arc::new(UdpSocket::from_std(sock_v6.into())?));

                (socket, socket_v6)
            },
        };

        let node_id = generate_random_id();
        let mut rng = rand::thread_rng();
        let token_secret: Vec<u8> = (0..10).map(|_| rng.r#gen::<u8>()).collect();

        // 使用分片队列
        // 队列容量：从配置获取
        let node_queue = ShardedNodeQueue::new(options.node_queue_capacity);

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
            socket,
            socket_v6,
            token_secret,
            callback,
            on_metadata_fetch,
            node_queue: Arc::new(node_queue),
            filter: Arc::new(RwLock::new(None)),
            hash_tx,
            metadata_queue_len,
            max_metadata_queue_size: options.max_metadata_queue_size,
        };

        Ok(server)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// 验证地址类型是否与当前 netmode 配置匹配
    /// 
    /// 防御性编程：虽然 socket 层面理论上不应该接收到不匹配的数据包，
    /// 但在某些特殊情况下（如系统配置、双栈模式切换等）可能会有问题。
    /// 此方法确保在应用层也进行验证，避免处理不匹配的地址类型。
    fn is_addr_allowed(&self, addr: &SocketAddr) -> bool {
        match self.options.netmode {
            NetMode::Ipv4Only => addr.is_ipv4(),
            NetMode::Ipv6Only => addr.is_ipv6(),
            NetMode::DualStack => true, // 双栈模式接受所有地址类型
        }
    }

    /// 根据目标地址选择合适的socket
    fn select_socket(&self, addr: &SocketAddr) -> &Arc<UdpSocket> {
        match self.options.netmode {
            NetMode::Ipv4Only => {
                // IPv4Only 模式：只有 IPv4 socket
                &self.socket
            },
            NetMode::Ipv6Only => {
                // IPv6Only 模式：只有 IPv6 socket
                &self.socket
            },
            NetMode::DualStack => {
                // 双栈模式：根据地址类型选择
                if addr.is_ipv6() {
                    self.socket_v6.as_ref().unwrap_or(&self.socket)
                } else {
                    &self.socket
                }
            },
        }
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
    /// ```rust,ignore
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
    /// ```rust,ignore
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
                // 🚀 自适应爬取速度：根据 Metadata 队列负载调整爬取策略
                let queue_len = server.metadata_queue_len.load(Ordering::Relaxed);
                let queue_pressure = queue_len as f64 / server.max_metadata_queue_size as f64;
                
                // 动态计算批次大小和休眠时间
                let (batch_size, sleep_duration) = if queue_pressure < 0.8 {
                    // 🟡 黄区：队列有压力，适度减速
                    (200, Duration::from_millis(10))
                } else if queue_pressure < 0.95 {
                    // 🟠 橙区：队列高压，大幅减速
                    (20, Duration::from_millis(500))
                } else {
                    // 🔴 红区：队列爆满，暂停主动爬取
                    (0, Duration::from_millis(1000))
                };

                // 根据配置决定从哪个队列获取节点
                let filter_ipv6 = match server.options.netmode {
                    NetMode::Ipv4Only => Some(false),
                    NetMode::Ipv6Only => Some(true),
                    NetMode::DualStack => None,
                };
                
                // 检查对应队列是否为空
                let queue_empty = server.node_queue.is_empty_for(filter_ipv6);
                
                let nodes_batch = {
                    if queue_empty || batch_size == 0 {
                        None
                    } else {
                        Some(server.node_queue.pop_batch(batch_size, filter_ipv6))
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
        let socket_v6 = self.socket_v6.clone();
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

        let senders_for_v6 = senders.clone();
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

        // IPv6 接收任务
        if let Some(socket_v6) = socket_v6 {
            let senders_v6 = senders_for_v6;
            tokio::spawn(async move {
                let mut buf = [0u8; 65536];
                let mut next_worker_idx = 0;

                loop {
                    match socket_v6.recv_from(&mut buf).await {
                        Ok((size, addr)) => {
                            if size > 8192 { continue; }
                            if size == 0 || buf[0] != b'd' { continue; }

                            let data = buf[..size].to_vec();

                            let tx = &senders_v6[next_worker_idx];
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
    }

    async fn handle_message(&self, data: &[u8], addr: SocketAddr) -> Result<()> {
        // 🛡️ 验证地址类型是否与当前 netmode 配置匹配
        // 防御性编程：虽然 socket 层面理论上不应该接收到不匹配的数据包，
        // 但在某些特殊情况下（如系统配置、双栈模式切换等）可能会有问题
        if !self.is_addr_allowed(&addr) {
            log::trace!("⚠️ 拒绝不匹配的地址类型: {} (当前模式: {:?})", addr, self.options.netmode);
            return Ok(());
        }

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
                    log::debug!("⚠️ Hash 队列满，丢弃 hash");
                }
            }
        }
        Ok(())
    }

    async fn handle_response(&self, response: &DhtResponse) -> Result<()> {
        // 处理 IPv4 节点
        if let Some(nodes_bytes) = &response.nodes {
            self.process_compact_nodes(nodes_bytes);
        }
        // 处理 IPv6 节点
        if let Some(nodes6_bytes) = &response.nodes6 {
            self.process_compact_nodes_v6(nodes6_bytes);
        }
        Ok(())
    }

    fn process_compact_nodes(&self, nodes_bytes: &[u8]) {
        // 根据配置决定是否处理IPv4节点
        if self.options.netmode == NetMode::Ipv6Only {
            return;
        }

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

    fn process_compact_nodes_v6(&self, nodes_bytes: &[u8]) {
        // 根据配置决定是否处理IPv6节点
        if self.options.netmode == NetMode::Ipv4Only {
            return;
        }

        if nodes_bytes.len() % 38 != 0 { return; }
        for chunk in nodes_bytes.chunks(38) {
            let id = chunk[0..20].to_vec();
            let port = u16::from_be_bytes([chunk[36], chunk[37]]);
            let ip_bytes: [u8; 16] = match chunk[20..36].try_into() {
                Ok(b) => b,
                Err(_) => continue, // 如果转换失败（理论上不会），跳过该节点
            };
            let ip = Ipv6Addr::from(ip_bytes);
            // 过滤掉不可用地址 (组播, 未指定等)
            if !ip.is_unspecified() && !ip.is_multicast() {
                let addr = SocketAddr::new(IpAddr::V6(ip), port);
                self.node_queue.push(NodeTuple { id, addr });
            }
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
            // 根据配置和请求方IP类型决定需要获取的节点类型
            let requestor_is_ipv6 = addr.is_ipv6();
            let filter_ipv6 = match self.options.netmode {
                NetMode::Ipv4Only => Some(false),  // 只要 IPv4
                NetMode::Ipv6Only => Some(true),   // 只要 IPv6
                NetMode::DualStack => Some(requestor_is_ipv6),  // 双栈模式：根据请求方IP类型返回对应类型的节点
            };
            
            // 使用分片队列获取随机节点（无锁竞争，带地址族过滤）
            let nodes = self.node_queue.get_random_nodes(8, filter_ipv6);
            
            let mut nodes_data = Vec::new();
            let mut nodes6_data = Vec::new();

            for node in nodes {
                match node.addr.ip() {
                    // IPv4 节点
                    IpAddr::V4(ip) => {
                        nodes_data.extend_from_slice(&node.id);
                        nodes_data.extend_from_slice(&ip.octets());
                        nodes_data.extend_from_slice(&node.addr.port().to_be_bytes());
                    },
                    // IPv6 节点
                    IpAddr::V6(ip) => {
                        nodes6_data.extend_from_slice(&node.id);
                        nodes6_data.extend_from_slice(&ip.octets());
                        nodes6_data.extend_from_slice(&node.addr.port().to_be_bytes());
                    },
                }
            }
            
            // 根据请求方IP类型返回对应类型的节点
            // 在单栈模式下，get_random_nodes 已经过滤了节点类型，所以这里直接根据请求方类型返回即可
            if requestor_is_ipv6 {
                // 请求方是IPv6：返回IPv6节点
                if !nodes6_data.is_empty() {
                    r_dict.insert(b"nodes6".to_vec(), serde_bencode::value::Value::Bytes(nodes6_data));
                }
            } else {
                // 请求方是IPv4：返回IPv4节点
                if !nodes_data.is_empty() {
                    r_dict.insert(b"nodes".to_vec(), serde_bencode::value::Value::Bytes(nodes_data));
                }
            }
        }

        let mut response: std::collections::HashMap<String, serde_bencode::value::Value> = std::collections::HashMap::new();
        response.insert("t".to_string(), serde_bencode::value::Value::Bytes(tid.to_vec()));
        response.insert("y".to_string(), serde_bencode::value::Value::Bytes(b"r".to_vec()));
        response.insert("r".to_string(), serde_bencode::value::Value::Dict(r_dict));

        if let Ok(encoded) = serde_bencode::to_bytes(&response) {
            let _ = self.select_socket(&addr).send_to(&encoded, addr).await;
        }
        Ok(())
    }

    async fn bootstrap(&self) {
        let target = generate_random_id();
        for node in BOOTSTRAP_NODES {
            match tokio::net::lookup_host(node).await {
                Ok(addrs) => {
                    for addr in addrs {
                        // 根据配置过滤地址
                        match self.options.netmode {
                            NetMode::Ipv4Only => {
                                if addr.is_ipv6() { continue; }
                            },
                            NetMode::Ipv6Only => {
                                if addr.is_ipv4() { continue; }
                            },
                            NetMode::DualStack => {
                                // 双栈模式，接受所有地址
                            },
                        }
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
            let _ = self.select_socket(&addr).send_to(&encoded, addr).await;
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
    (0..20).map(|_| rng.r#gen::<u8>()).collect()
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
