#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use dht_crawler::prelude::*;
#[cfg(feature = "metrics")]
use metrics_exporter_prometheus::PrometheusBuilder;
#[cfg(feature = "metrics")]
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing_subscriber::EnvFilter;
#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(true)
        .init();

    // 初始化 Prometheus metrics 导出器
    #[cfg(feature = "metrics")]
    {
        let addr: SocketAddr = "0.0.0.0:9000"
            .parse()
            .map_err(|e| DHTError::Init(format!("无效的 metrics 监听地址: {e}")))?;
        PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .map_err(|e| DHTError::Init(format!("无法安装 Prometheus metrics 导出器: {e}")))?;
        log::info!("📊 Prometheus metrics 导出器已启动，访问 http://localhost:9000/metrics");
    }

    let options = DHTOptions {
        port: 12313,
        netmode: NetMode::Ipv4Only,
        metadata: MetadataOptions {
            timeout_secs: 4,
            max_queue_size: 10_000,
            max_worker_count: 256,
            ..MetadataOptions::default()
        },
        ..Default::default()
    };

    // 统计计数器
    let torrent_count = Arc::new(AtomicUsize::new(0));
    let torrent_count_clone = torrent_count.clone();

    // 🚀 初始化 DHT Server
    log::info!("🔧 正在初始化 DHT Server...");
    let server = DHTServer::new(options.clone()).await?;

    log::info!("🚀 DHT Server 启动，监听端口: {}", options.port);

    // 注册错误回调，将运行时错误输出而不是 panic
    server.on_error(|err| {
        log::error!("DHT 运行时错误: {}", err);
    });

    // 设置 torrent 回调
    server.on_torrent(move |_torrent| {
        let _count = torrent_count_clone.fetch_add(1, Ordering::Relaxed) + 1;

        // 🔇 取消打印 torrent 信息，减少日志输出
        // let total_size: u64 = torrent.files.iter().map(|f| f.size).sum();
        // let files_display = if torrent.files.len() <= 3 {
        //     torrent.files.iter()
        //         .map(|f| format!("{} ({})", f.path, format_size(f.size)))
        //         .collect::<Vec<_>>()
        //         .join(", ")
        // } else {
        //     format!("{}个文件", torrent.files.len())
        // };
        //
        // log::info!(
        //     "🎉 [{}] {} ({}, {})",
        //     count,
        //     torrent.name,
        //     format_size(total_size),
        //     files_display
        // );
    });

    // 设置元数据获取前的检查回调
    server.on_metadata_fetch(|_hash| async move { true });

    // 一个通过 gate 的 Hash 最终只会收到一次完成状态。
    server.on_metadata_fetch_complete(|completion| {
        log::debug!(
            "Metadata complete: hash={}, status={:?}, peer_attempts={}",
            completion.info_hash,
            completion.status,
            completion.attempts
        );
    });

    // 启动监控任务
    let count_monitor = torrent_count.clone();
    let runtime_stats = server.runtime_stats();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let start_time = std::time::Instant::now();

        loop {
            interval.tick().await;
            let success_fetch = count_monitor.load(Ordering::Relaxed);
            let uptime = start_time.elapsed().as_secs();
            let runtime = runtime_stats.snapshot();

            // ✅ 监控：爬虫运行状态
            log::info!(
                "📊 [监控] 时长: {}s | 成功抓取: ✨ {} | 节点: {} | Metadata: {}/{} | worker: {}",
                uptime,
                success_fetch,
                runtime.node_pool_size,
                runtime.metadata_queue_depth,
                runtime.metadata_queue_max,
                runtime.metadata_in_flight,
            );

            if uptime > 0 && success_fetch > 0 {
                let speed = (success_fetch as f64) / (uptime as f64 / 60.0);
                log::info!("📈 平均抓取速度: {:.2} 种子/分钟", speed);
            }
        }
    });

    let shutdown_server = server.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            log::info!("收到 Ctrl-C，正在停止 DHT Server");
            shutdown_server.shutdown();
        }
    });

    server.start().await?;
    Ok(())
}
