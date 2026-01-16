# dht_crawler

一个高性能的 Rust DHT（分布式哈希表）爬虫库，用于爬取 BitTorrent DHT 网络中的种子信息。

## 特性

- 🚀 **高性能**：基于 Tokio 异步运行时，支持高并发处理
- 🌐 **双栈支持**：同时支持 IPv4 和 IPv6（DualStack 模式）
- 📦 **自动元数据获取**：自动从对等节点获取完整的种子元数据
- ⚡ **可配置并发**：支持自定义元数据获取并发数和队列大小
- 🎯 **灵活回调**：提供多种回调接口，方便自定义处理逻辑
- 📊 **监控支持**：内置统计和监控接口

## 安装

在 `Cargo.toml` 中添加依赖：

```toml
[dependencies]
dht-crawler = "0.0.1"
```

## 快速开始

### 基本使用

```rust
use dht_crawler::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // 配置选项
    let options = DHTOptions {
        port: 6881,
        auto_metadata: true,
        metadata_timeout: 3,
        max_metadata_queue_size: 10000,
        max_metadata_worker_count: 500,
        netmode: NetMode::DualStack,
        ..Default::default()
    };

    // 创建 DHT 服务器
    let server = DHTServer::new(options).await?;

    // 设置种子发现回调
    server.on_torrent(|torrent| {
        println!("发现种子: {} ({})", torrent.name, torrent.format_size());
    });

    // 启动服务器
    server.start().await?;
    Ok(())
}
```

## 性能优化

推荐使用 mimalloc 分配器以获得更好的内存性能：

```bash
cargo build --release --features mimalloc
```

使用 mimalloc 可以显著降低内存占用（通常降低 10-30%）。

## 许可证

MIT License
