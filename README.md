# dht-crawler

[![Crates.io](https://img.shields.io/crates/v/dht-crawler.svg)](https://crates.io/crates/dht-crawler)
[![Documentation](https://docs.rs/dht-crawler/badge.svg)](https://docs.rs/dht-crawler)
[![License](https://img.shields.io/crates/l/dht-crawler.svg)](LICENSE)

基于 Rust 和 Tokio 的 BitTorrent DHT 爬虫库。它参与 BEP-5 DHT 网络，接收
`announce_peer`，并通过 BEP-9 `ut_metadata` 获取、校验和解析 torrent 元数据。

`dht-crawler` 提供：

- IPv4、IPv6 和双栈 DHT；
- 主动节点发现与 `get_peers` 查询；
- 有界、去重的 Metadata 下载队列；
- InfoHash 过滤、异步准入、结果交付和完成通知；
- 默认可用的运行时统计，以及可选的 `metrics` 集成。

## 安装

```bash
cargo add dht-crawler
cargo add tokio --features rt-multi-thread,macros,signal
```

或在 `Cargo.toml` 中添加：

```toml
[dependencies]
dht-crawler = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

## 快速开始

```rust
use dht_crawler::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let server = DHTServer::new(DHTOptions {
        port: 6881,
        netmode: NetMode::Ipv4Only,
        ..Default::default()
    })
    .await?;

    server.on_torrent(|torrent| {
        println!(
            "{}  {}  {}",
            torrent.info_hash,
            torrent.name,
            torrent.format_size()
        );
    });

    server.on_error(|error| {
        eprintln!("DHT runtime error: {error}");
    });

    let shutdown = server.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown.shutdown();
        }
    });

    // 一直运行，直到 shutdown() 被调用。
    server.start().await
}
```

运行仓库中的完整示例：

```bash
cargo run --release --example dht_crawler_example
```

## 核心 API

`DHTServer` 是主要入口：

| API | 用途 |
|---|---|
| `DHTServer::new(options)` | 校验配置、绑定 UDP Socket 并创建内部管道 |
| `start().await` | 启动 DHT 与爬取任务，等待 `shutdown()` |
| `shutdown()` | 停止 UDP、爬取和 Metadata 任务；可重复调用 |
| `filter(callback)` | 在 InfoHash 进入队列前执行同步过滤 |
| `on_metadata_fetch(callback)` | 在第一次 Peer 下载前执行异步准入 |
| `on_torrent(callback)` | 接收已校验的 `TorrentInfo` |
| `on_torrent_with_ack(callback)` | 接收结果并显式确认是否接受交付 |
| `on_metadata_fetch_complete(callback)` | 接收已准入任务的最终状态 |
| `on_error(callback)` | 接收运行期错误 |
| `runtime_stats()` | 获取可复制的运行时统计句柄 |

同类回调重复注册时，新回调会替换旧回调。

### 过滤与准入

`filter` 是同步的早期过滤器，适合拦截已处理过的 InfoHash：

```rust
server.filter(|info_hash| !already_exists(info_hash));
```

`on_metadata_fetch` 是异步准入回调，在实际连接 Peer 前调用：

```rust
server.on_metadata_fetch(|info_hash| async move {
    should_download(&info_hash).await
});
```

返回 `false` 会终止任务，不下载 Metadata，也不会触发 torrent 或 completion 回调。
未注册准入回调时默认允许下载。

### 交付确认

不需要确认下游是否接收时使用 `on_torrent`。需要确认下游是否成功接收时使用
`on_torrent_with_ack`：

```rust
server.on_torrent_with_ack(|torrent| {
    output.try_send(torrent).is_ok()
});

server.on_metadata_fetch_complete(|completion| {
    println!(
        "{}: {:?}, attempts={}",
        completion.info_hash,
        completion.status,
        completion.attempts
    );
});
```

完成状态：

| 状态 | 含义 |
|---|---|
| `Accepted` | Metadata 下载成功，结果已被回调接受 |
| `FetchFailed` | 所有可用 Peer 尝试均失败 |
| `DeliveryRejected` | Metadata 下载成功，但结果未被回调接受 |

`attempts` 只统计实际发起的 Peer 网络请求。异步准入拒绝不会产生 completion 事件。

## 配置

大多数调用方可以从 `DHTOptions::default()` 开始，只覆盖监听方式和容量限制：

```rust
let options = DHTOptions {
    port: 6881,
    netmode: NetMode::DualStack,
    hash_queue_capacity: 20_000,
    metadata: MetadataOptions {
        timeout_secs: 5,
        max_queue_size: 20_000,
        max_worker_count: 256,
        ..Default::default()
    },
    crawl: CrawlOptions {
        rate_limit: RateLimitOptions {
            max_find_node_rate_per_sec: 300,
            max_in_flight: 768,
            ..Default::default()
        },
        ..Default::default()
    },
    ..Default::default()
};
```

配置分组：

| 类型 | 控制内容 |
|---|---|
| `DHTOptions` | 监听端口、网络模式和顶层队列 |
| `MetadataOptions` | 下载超时、队列、并发和失败 Peer 缓存 |
| `PeerLookupOptions` | 主动 `get_peers` 的速率与并发 |
| `RateLimitOptions` | `find_node`、在途请求和 UDP 回复预算 |
| `PoolOptions` | 节点池、最近探测记录和响应节点缓存 |
| `BootstrapOptions` | Bootstrap 节点与失败退避 |
| `TargetOptions` | 主动爬取目标生成策略 |
| `SchedulerOptions` | 内部事件队列、批处理与快照限制 |

完整字段和默认值以 [docs.rs API 文档](https://docs.rs/dht-crawler) 为准。需要注意：

- `DHTOptions::default()` 使用 `Ipv4Only`；
- `NetMode::DualStack` 会分别绑定 IPv4 和 IPv6 Socket；
- `DHTServer::new()` 会立即在所有可用接口上绑定配置的 UDP 端口；
- `MetadataOptions::timeout_secs` 是单个 Peer 尝试的端到端期限；
- `PeerLookupOptions::max_lookups_per_second = 0` 会关闭主动 `get_peers`；
- Metadata 和爬取队列都是有界的，容量应与下游处理能力一起调整。

## 数据与运行语义

`TorrentInfo` 包含 `info_hash`、`magnet_link`、`name`、`total_size`、`files`、
`piece_length`、`peers` 和 `timestamp`。只有通过 SHA1 校验并成功解析的 Metadata
才会交付给 torrent 回调。

库使用有界队列控制内存占用。队列满或速率预算耗尽时，新事件可能被拒绝、淘汰或计入
drop 指标。Metadata 队列按 InfoHash 去重，每个任务可尝试多个候选 Peer；连接超时和
连接失败的 Peer 会被短期缓存，避免反复占用 worker。

`start()` 返回后，当前实例不能再次启动。如需重新运行，请创建新的 `DHTServer`。

## 可观测性

运行时快照无需启用 Cargo feature：

```rust
let stats = server.runtime_stats();
let snapshot = stats.snapshot();

println!(
    "nodes={} metadata={}/{} workers={}",
    snapshot.node_pool_size,
    snapshot.metadata_queue_depth,
    snapshot.metadata_queue_max,
    snapshot.metadata_in_flight,
);
```

`observability_snapshot()` 提供 UDP、查询、队列、Metadata 失败原因和固定桶直方图。
这些快照面向监控，读取时不是跨字段事务视图。

启用 `metrics` 后，库通过 [`metrics`](https://crates.io/crates/metrics) facade 记录
指标，但不会安装 recorder 或启动 HTTP 服务：

```toml
[dependencies]
dht-crawler = { version = "0.2", features = ["metrics"] }
```

指标名称、类型、标签和单位见 [docs/metrics.md](docs/metrics.md)。

## Cargo features

默认不启用任何 feature。

| Feature | 用途 |
|---|---|
| `metrics` | 通过 `metrics` facade 记录指标 |
| `jni` | 构建 Java JNI 接口和 `cdylib` |
| `mimalloc` | 将 mimalloc 注册为全局分配器 |

Java/Kotlin 封装、平台 native JAR 和示例由独立项目
[`dht-crawler-java`](https://github.com/0xddy/dht-crawler-java) 维护。
启用 `mimalloc` 前，请确认最终二进制没有注册其他全局分配器。

## 开发

```bash
cargo fmt --all --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo doc --no-deps --all-features
```

## 许可证

[MIT](LICENSE)
