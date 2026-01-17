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

pub struct MetadataScheduler {
    hash_rx: mpsc::Receiver<HashDiscovered>,
    max_queue_size: usize,
    max_concurrent: usize,
    fetcher: Arc<RbitFetcher>,
    callback: Arc<RwLock<Option<TorrentCallback>>>,
    on_metadata_fetch: Arc<RwLock<Option<MetadataFetchCallback>>>,
    total_received: Arc<AtomicU64>,
    total_dropped: Arc<AtomicU64>,
    total_dispatched: Arc<AtomicU64>,
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
        queue_len: Arc<AtomicUsize>,
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
    
    pub fn set_callback(&mut self, callback: TorrentCallback) {
        if let Ok(mut guard) = self.callback.try_write() {
            *guard = Some(callback);
        }
    }
    
    pub fn set_metadata_fetch_callback(&mut self, callback: MetadataFetchCallback) {
        if let Ok(mut guard) = self.on_metadata_fetch.try_write() {
            *guard = Some(callback);
        }
    }
    
    pub async fn run(mut self) {        
        let (task_tx, task_rx) = mpsc::channel::<HashDiscovered>(self.max_queue_size);
        let task_rx = Arc::new(Mutex::new(task_rx));
        
        for worker_id in 0..self.max_concurrent {
            let task_rx = task_rx.clone();
            let fetcher = self.fetcher.clone();
            let callback = self.callback.clone();
            let on_metadata_fetch = self.on_metadata_fetch.clone();
            let total_dispatched = self.total_dispatched.clone();
            let queue_len = self.queue_len.clone();
            
            tokio::spawn(async move {
                log::trace!("Worker {} 启动", worker_id);
                
                loop {
                    let hash = {
                        let mut rx = task_rx.lock().await;
                        let h = rx.recv().await;
                        if h.is_some() {
                            queue_len.fetch_sub(1, Ordering::Relaxed);
                        }
                        h
                    };
                    
                    let hash = match hash {
                        Some(h) => h,
                        None => break,
                    };
                    
                    total_dispatched.fetch_add(1, Ordering::Relaxed);
                    
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
                        
                        match task_tx.try_send(hash) {
                            Ok(_) => {
                                self.queue_len.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => break,
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
                        
                        match task_tx.try_send(hash) {
                            Ok(_) => {
                                self.queue_len.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => break,
                        }
                    }
                    None => break,
                }
            }
        }
    }
    
    async fn process_hash(
        hash: HashDiscovered,
        fetcher: &Arc<RbitFetcher>,
        callback: &Arc<RwLock<Option<TorrentCallback>>>,
        on_metadata_fetch: &Arc<RwLock<Option<MetadataFetchCallback>>>,
    ) {
        let info_hash = hash.info_hash.clone();
        let peer_addr = hash.peer_addr;
        
        let maybe_check_fn = {
            match on_metadata_fetch.read() {
                Ok(guard) => guard.clone(),
                Err(_) => return,
            }
        };

        if let Some(f) = maybe_check_fn {
            if !f(info_hash.clone()).await {
                return;
            }
        }
        
        let info_hash_bytes: [u8; 20] = match hex::decode(&info_hash) {
            Ok(bytes) if bytes.len() == 20 => {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                arr
            }
            _ => return,
        };
        
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
            
            let maybe_torrent_cb = {
                match callback.read() {
                    Ok(guard) => guard.clone(),
                    Err(_) => return,
                }
            };
            
            if let Some(cb) = maybe_torrent_cb {
                cb(metadata);
            }
        }
    }
    
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
