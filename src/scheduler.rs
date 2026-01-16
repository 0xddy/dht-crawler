use crate::server::HashDiscovered;
use crate::types::TorrentInfo;
use crate::metadata::RbitFetcher;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{mpsc, Mutex};
#[cfg(debug_assertions)]
use std::time::Duration;

type TorrentCallback = Arc<dyn Fn(TorrentInfo) + Send + Sync>;
type MetadataFetchCallback = Arc<dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync>;

/// 元数据调度器（优雅版：Worker 池 + Channel）
/// 负责管理元数据获取队列和任务调度
pub struct MetadataScheduler {
    // 输入通道
    hash_rx: mpsc::Receiver<HashDiscovered>,
    
    // 配置
    max_queue_size: usize,
    max_concurrent: usize,
    
    // 元数据获取器
    fetcher: Arc<RbitFetcher>,
    
    // 回调
    callback: Arc<RwLock<Option<TorrentCallback>>>,
    on_metadata_fetch: Arc<RwLock<Option<MetadataFetchCallback>>>,
    
    // 统计（使用 Atomic 支持多线程访问）
    total_received: Arc<AtomicU64>,
    total_dropped: Arc<AtomicU64>,
    total_dispatched: Arc<AtomicU64>,
    
    // 共享的队列长度计数器（用于向 Server 反馈背压）
    queue_len: Arc<AtomicUsize>,
}

impl MetadataScheduler {
    pub fn new(
        hash_rx: mpsc::Receiver<HashDiscovered>,
        fetcher: Arc<RbitFetcher>,
        max_queue_size: usize,
        max_concurrent: usize,
        callback: Arc<RwLock<Option<TorrentCallback>>>,
        on_metadata_fetch: Arc<RwLock<Option<MetadataFetchCallback>>>,
        queue_len: Arc<AtomicUsize>, // 新增参数
    ) -> Self {
        Self {
            hash_rx,
            max_queue_size,
            max_concurrent,
            fetcher,
            callback,
            on_metadata_fetch,
            total_received: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
            total_dispatched: Arc::new(AtomicU64::new(0)),
            queue_len,
        }
    }
    
    /// 设置 torrent 回调
    pub fn set_callback(&mut self, callback: TorrentCallback) {
        if let Ok(mut guard) = self.callback.try_write() {
            *guard = Some(callback);
        }
    }
    
    /// 设置元数据获取前的检查回调
    pub fn set_metadata_fetch_callback(&mut self, callback: MetadataFetchCallback) {
        if let Ok(mut guard) = self.on_metadata_fetch.try_write() {
            *guard = Some(callback);
        }
    }
    
    /// 运行调度器（完全事件驱动）
    pub async fn run(mut self) {        
        // 创建任务队列（channel 自带背压）
        let (task_tx, task_rx) = mpsc::channel::<HashDiscovered>(self.max_queue_size);
        let task_rx = Arc::new(Mutex::new(task_rx));
        
        // 启动 Worker 池
        for worker_id in 0..self.max_concurrent {
            let task_rx = task_rx.clone();
            let fetcher = self.fetcher.clone();
            let callback = self.callback.clone();
            let on_metadata_fetch = self.on_metadata_fetch.clone();
            let total_dispatched = self.total_dispatched.clone();
            let queue_len = self.queue_len.clone(); // 传递计数器
            
            tokio::spawn(async move {
                log::trace!("Worker {} 启动", worker_id);
                
                loop {
                    // Worker 从队列取任务（阻塞等待，零延迟）
                    let hash = {
                        let mut rx = task_rx.lock().await;
                        let h = rx.recv().await;
                        // 取出任务后，减少计数器
                        if h.is_some() {
                            queue_len.fetch_sub(1, Ordering::Relaxed);
                        }
                        h
                    };
                    
                    let hash = match hash {
                        Some(h) => h,
                        None => break,  // Channel 关闭，退出
                    };
                    
                    total_dispatched.fetch_add(1, Ordering::Relaxed);
                    
                    // 执行任务
                    Self::process_hash(
                        hash,
                        &fetcher,
                        &callback,
                        &on_metadata_fetch,
                    ).await;
                }
                
                log::trace!("Worker {} 退出", worker_id);
            });
        }
        
        // 主循环：只负责接收 hash 并转发到 worker 队列
        #[cfg(debug_assertions)]
        let mut stats_interval = tokio::time::interval(Duration::from_secs(60));
        #[cfg(debug_assertions)]
        stats_interval.tick().await;
        
        loop {
            #[cfg(debug_assertions)]
            {
                tokio::select! {
                    Some(hash) = self.hash_rx.recv() => {
                        self.total_received.fetch_add(1, Ordering::Relaxed);
                        
                        // 尝试发送到 worker 队列
                        match task_tx.try_send(hash) {
                            Ok(_) => {
                                // 成功入队，增加计数器
                                self.queue_len.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // 队列满，丢弃
                                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => break,  // Channel 关闭
                        }
                    }
                    
                    _ = stats_interval.tick() => {
                        self.print_stats(&task_tx);
                    }
                    
                    else => break,
                }
            }
            
            #[cfg(not(debug_assertions))]
            {
                match self.hash_rx.recv().await {
                    Some(hash) => {
                        self.total_received.fetch_add(1, Ordering::Relaxed);
                        
                        // 尝试发送到 worker 队列
                        match task_tx.try_send(hash) {
                            Ok(_) => {
                                // 成功入队，增加计数器
                                self.queue_len.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // 队列满，丢弃
                                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => break,  // Channel 关闭
                        }
                    }
                    None => break,  // Channel 关闭
                }
            }
        }
    }
    
    /// 处理单个 hash（Worker 调用）
    async fn process_hash(
        hash: HashDiscovered,
        fetcher: &Arc<RbitFetcher>,
        callback: &Arc<RwLock<Option<TorrentCallback>>>,
        on_metadata_fetch: &Arc<RwLock<Option<MetadataFetchCallback>>>,
    ) {
        let info_hash = hash.info_hash.clone();
        let peer_addr = hash.peer_addr;
        
        // 检查是否需要获取（获取回调快照并释放锁）
        let maybe_check_fn = {
            match on_metadata_fetch.read() {
                Ok(guard) => guard.clone(),
                Err(_) => return, // 锁中毒
            }
        };

        if let Some(f) = maybe_check_fn {
            if !f(info_hash.clone()).await {
                return;
            }
        }
        
        // 解码 info_hash
        let info_hash_bytes: [u8; 20] = match hex::decode(&info_hash) {
            Ok(bytes) if bytes.len() == 20 => {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                arr
            }
            _ => return,
        };
        
        // 获取元数据
        if let Some((name, total_size, files)) = fetcher.fetch(&info_hash_bytes, peer_addr).await {
            let metadata = TorrentInfo {
                info_hash,
                name,
                total_size,
                files,
                magnet_link: format!("magnet:?xt=urn:btih:{}", hash.info_hash),
                peers: vec![peer_addr.to_string()],
                piece_length: 0,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };
            
            // 获取回调快照并释放锁
            let maybe_torrent_cb = {
                match callback.read() {
                    Ok(guard) => guard.clone(),
                    Err(_) => return, // 锁中毒
                }
            };
            
            if let Some(cb) = maybe_torrent_cb {
                cb(metadata);
            }
        }
    }
    
    /// 输出统计信息（仅在 debug 模式下编译）
    #[cfg(debug_assertions)]
    fn print_stats(&self, task_tx: &mpsc::Sender<HashDiscovered>) {
        let received = self.total_received.load(Ordering::Relaxed);
        let dropped = self.total_dropped.load(Ordering::Relaxed);
        let dispatched = self.total_dispatched.load(Ordering::Relaxed);
        
        let drop_rate = if received > 0 {
            dropped as f64 / received as f64 * 100.0
        } else {
            0.0
        };
        
        let queue_size = self.max_queue_size - task_tx.capacity();
        let queue_pressure = (queue_size as f64 / self.max_queue_size as f64) * 100.0;
        
        // 根据压力选择日志级别
        if queue_pressure > 80.0 {
            log::warn!(
                "⚠️ Metadata 队列高压：队列={}/{}({:.1}%), 接收={}, 调度={}, 丢弃={}({:.2}%)",
                queue_size,
                self.max_queue_size,
                queue_pressure,
                received,
                dispatched,
                dropped,
                drop_rate
            );
        } else {
            log::info!(
                "📊 Metadata 调度器统计：队列={}/{}({:.1}%), 接收={}, 调度={}, 丢弃={}({:.2}%)",
                queue_size,
                self.max_queue_size,
                queue_pressure,
                received,
                dispatched,
                dropped,
                drop_rate
            );
        }
    }
}
