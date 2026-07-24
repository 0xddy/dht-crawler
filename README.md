# dht-crawler

[![Crates.io](https://img.shields.io/crates/v/dht-crawler.svg)](https://crates.io/crates/dht-crawler)
[![Documentation](https://docs.rs/dht-crawler/badge.svg)](https://docs.rs/dht-crawler)
[![License](https://img.shields.io/crates/l/dht-crawler.svg)](LICENSE)

基于 Rust、Tokio 的 BitTorrent DHT 爬虫库。它加入 BEP-5 网络，接收有效的
`announce_peer`，并通过 BEP-9 `ut_metadata` 下载和校验种子元数据。

当前版本：`0.2.0`。0.2 重做了节点池、主动爬取、Metadata 调度和运行时观测接口，
从 0.1 升级时请先阅读[迁移说明](#从-01-迁移到-02)和 [CHANGELOG](CHANGELOG.md)。

## 文档导航

- [快速开始](#快速开始)：最小可运行示例和优雅停机。
- [架构与背压](#架构与背压)：UDP、主动爬取和 Metadata 管道。
- [配置默认值](#配置默认值)：所有公开 `DHTOptions` 字段。
- [回调与生命周期](#回调与生命周期)：抓取准入、交付确认和完成状态。
- [运行时观测](#运行时观测)：无 exporter 快照、Prometheus 和完整[指标表](docs/metrics.md)。
- [JNI](#jni)：Java 集成入口；[0.1 → 0.2 迁移](#从-01-迁移到-02)。

## 主要能力

- IPv4、IPv6 和双栈 DHT Socket。
- 单所有者 crawl actor：严格 FIFO 节点池、最近探测状态、在途请求和所有速率预算
  由一个 actor 管理，UDP worker 不锁节点池。
- 查询 QPS、新目标/分钟、节点替换/分钟、总在途、子网在途、回复包、回复字节和
  单来源回复分别限流。
- 有界 Metadata 队列按 InfoHash 去重，并保留最多三个新鲜 Peer。
- Metadata 总超时覆盖 TCP 连接、BitTorrent/扩展握手、分片下载、SHA1 校验和解析。
- 按 `SocketAddr` 缓存 Peer 的超时/连接失败，避免坏 Peer 反复占用 worker。
- 传输无关的原子运行时快照和固定桶直方图；可选 `metrics` feature。
- 可选 JNI 接口和 Java 示例。

## 安装

```toml
[dependencies]
dht-crawler = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

如果应用需要通过 `metrics` facade 输出指标：

```toml
[dependencies]
dht-crawler = { version = "0.2", features = ["metrics"] }
metrics-exporter-prometheus = { version = "0.18", default-features = false, features = ["http-listener"] }
```

`metrics` feature 只负责记录指标，不会在库内启动 HTTP 服务。应用必须自行安装
recorder/exporter；完整指标清单见 [docs/metrics.md](docs/metrics.md)。

### Cargo features

| Feature | 默认启用 | 作用 |
|---|---|---|
| `metrics` | 否 | 通过 `metrics` facade 记录低基数指标 |
| `jni` | 否 | 构建 Java JNI 接口和 `cdylib` |
| `mimalloc` | 否 | 将 mimalloc 注册为全局分配器 |

库的默认 feature 集为空。启用 `mimalloc` 前，请确认最终二进制没有注册其他全局分配器。

## 快速开始

```rust
use dht_crawler::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let options = DHTOptions {
        port: 12313,
        netmode: NetMode::Ipv4Only,
        metadata: MetadataOptions {
            timeout_secs: 4,
            max_queue_size: 10_000,
            max_worker_count: 256,
            ..Default::default()
        },
        crawl: CrawlOptions {
            rate_limit: RateLimitOptions {
                max_find_node_rate_per_sec: 200,
                burst: 40,
                max_in_flight: 512,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let server = DHTServer::new(options).await?;

    // 返回 true 才允许该 InfoHash 进入实际 Peer 下载阶段。
    server.on_metadata_fetch(|_info_hash| async move { true });

    // 简单回调总是接受交付。
    server.on_torrent(|torrent| {
        println!("{}: {}", torrent.info_hash, torrent.name);
    });

    server.on_error(|error| eprintln!("DHT runtime error: {error}"));

    let shutdown_server = server.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown_server.shutdown();
        }
    });

    // start() 阻塞到 shutdown() 被调用。
    server.start().await
}
```

可运行版本见 [examples/main.rs](examples/main.rs)。如果自己的 Tokio 依赖没有启用
`signal` feature，可以使用其他取消源调用 `shutdown()`。

## 架构与背压

```text
UDP sockets
   ├─ bounded UDP worker queues ──→ KRPC workers ──→ bounded crawl events
   │                                      │
   │                                      └─ announce_peer → bounded hash ingress
   │
   └─ crawl egress ← single crawl actor ← priority/discovery events
                         │
                         ├─ strict FIFO node pool + recent-probe set
                         ├─ pending transaction map + subnet counters
                         └─ ArcSwap responsive-node snapshot

hash ingress → deduplicating Metadata queue → bounded workers → torrent callback
```

所有跨任务入口都是有界队列。达到容量时，事件会被拒绝、淘汰或计入 drop 指标，
不会依靠无限增长的缓冲区掩盖下游过载。主动爬取预算还会随 Metadata 队列压力下降。

### 主动爬取

- 新地址通常只发送一次 `find_node`；默认等待回复 `2s`，不做同目标重试。
- 超时不会自动降低配置 QPS。Metadata 队列压力达到 80% 后才开始自动降速，95% 时
  降到 `metadata_pressure_floor_percent` 指定的比例；默认下限为配置 QPS 的 25%。
- 节点池是严格 FIFO。重复地址、无效公网地址和超出 replacement budget 的替换会被拒绝。
- 响应成功的节点进入一个独立、有界、带 TTL 的 responsive ring，用于回复其他 DHT
  节点和 revisit 查询；它不是第二个爬取池。
- 节点池低于 `low_watermark` 时触发 bootstrap。失败的 bootstrap 来源按配置退避。
- UDP 回复总包数、总字节数和单来源包数分别限流，其中 10% 包/字节预算保留给
  `ping` 和 `get_peers` 的保底回复，但不会突破配置的总上限。

### Metadata 调度

- Hash ingress 和 Metadata pending queue 都是有界的。
- Pending queue 按 InfoHash 去重，每个 Hash 最多保留三个不同且新鲜的 Peer。
- Pending 项固定在 60 秒后过期。队列满时，较新的 Hash 可以淘汰最旧项；比当前
  最旧项还旧的事件直接视为 stale。
- worker 优先分派最新的可用 Hash，以提高 Peer 仍在线的概率。
- `timeout_secs` 是一次 Peer 尝试的端到端期限，不会在连接、握手和下载阶段重复叠加。
- 单个 metadata payload 上限为 10 MiB；下载完成后必须通过 SHA1 和 bencode 解析。
- Peer failure cache 只缓存 `timeout` 和 `connect_failed`，键为完整 `SocketAddr`
  （IP + port）。缓存命中不会发起网络请求，也不计入三次真实 Peer 尝试。
- `peer_failure_cache_capacity = 0` 或 `peer_failure_ttl_secs = 0` 会关闭缓存。

## 配置默认值

库本身不包含 P1/P15 等档位概念。应用如需档位，应将其转换成下列具体选项。

### `DHTOptions` 与 Metadata

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `port` | `6881` | DHT UDP 监听端口 |
| `netmode` | `Ipv4Only` | `DHTOptions::default()` 的网络模式 |
| `hash_queue_capacity` | `10000` | announce 到 Metadata scheduler 的 ingress 容量 |
| `metadata.timeout_secs` | `4` | 单 Peer 端到端超时 |
| `metadata.max_queue_size` | `10000` | 去重 Pending Hash 容量 |
| `metadata.max_worker_count` | `256` | 最大并发 Metadata job 数 |
| `metadata.peer_failure_cache_capacity` | `200000` | 坏 Peer 缓存容量 |
| `metadata.peer_failure_ttl_secs` | `60` | 坏 Peer 缓存 TTL |
| `peer_lookup.max_lookups_per_second` | `32` | 每秒启动的主动 `get_peers` 查询数；`0` 为关闭 |
| `peer_lookup.burst` | `32` | 空闲后可立即消费的查询预算 |
| `peer_lookup.max_active_lookups` | `64` | 同时活跃的 InfoHash 查询上限 |

### `crawl.rate_limit`

| 字段 | 默认值 |
|---|---:|
| `max_find_node_rate_per_sec` | `200` |
| `burst` | `40` |
| `max_in_flight` | `512` |
| `request_timeout_secs` | `2` |
| `max_new_destinations_per_minute` | `10000` |
| `max_replacements_per_minute` | `25000` |
| `max_response_rate_per_sec` | `500` |
| `max_response_bytes_per_sec` | `1048576` |
| `max_response_rate_per_source` | `40` |
| `metadata_pressure_floor_percent` | `25` |
| `max_in_flight_per_subnet` | `8` |

### Pool、Bootstrap、Target 与 Scheduler

| 字段 | 默认值 |
|---|---:|
| `pool.capacity` | `100000` |
| `pool.recent_probe_ttl_secs` | `600` |
| `pool.responsive_capacity` | `16384` |
| `pool.responsive_ttl_secs` | `900` |
| `pool.low_watermark` | `10000` |
| `bootstrap.interval_secs` | `300` |
| `bootstrap.max_nodes_per_round` | `3` |
| `bootstrap.source_backoff_base_secs` | `300` |
| `bootstrap.source_backoff_max_secs` | `3600` |
| `target.random_walk_percent` | `70` |
| `target.sparse_bucket_percent` | `30` |
| `target.neighbor_sender_id` | `true` |
| `scheduler.priority_event_channel_capacity` | `8192` |
| `scheduler.discovery_event_channel_capacity` | `16384` |
| `scheduler.event_batch_limit` | `256` |
| `scheduler.node_batch_limit` | `4096` |
| `scheduler.routing_snapshot_size` | `4096` |
| `scheduler.snapshot_refresh_millis` | `1000` |

默认 bootstrap 来源：

```text
router.bittorrent.com:6881
dht.transmissionbt.com:6881
router.utorrent.com:6881
dht.aelitis.com:6881
```

内部会对不安全的零值和百分比做归一化，例如容量/在途至少为 1、百分比最大为 100、
`low_watermark` 不超过 pool capacity。建议调用方仍显式传入有效配置，不依赖归一化。

## 回调与生命周期

### `on_metadata_fetch`

在 Hash 首次准备进入 Peer 下载前调用。返回 `false` 表示 gate reject：不下载、不触发
`on_torrent`，也不会触发 `on_metadata_fetch_complete`。

### `on_torrent` 与 `on_torrent_with_ack`

- `on_torrent` 适合无需确认交付的调用方；回调返回后视为 `Accepted`。
- `on_torrent_with_ack` 返回 `true` 表示应用接受交付，返回 `false` 表示
  `DeliveryRejected`。后者代表 Metadata 已成功下载，但业务层没有接收，不等同于
  `FetchFailed`。
- 如果没有注册任何 torrent callback，成功下载的 Metadata 同样按 `DeliveryRejected`
  结束。
- Torrent 回调发生 panic 时会被捕获并按拒绝交付处理。

### `on_metadata_fetch_complete`

一个通过 gate 的 Hash 最终只发出一次完成通知：

| 状态 | 含义 |
|---|---|
| `Accepted` | 下载成功，torrent callback 接受交付 |
| `FetchFailed` | 所有可用 Peer 尝试失败 |
| `DeliveryRejected` | 下载成功，torrent callback 拒绝交付 |

`attempts` 只统计实际发起的 Peer 网络尝试；failure cache 命中不计入。

### 启动与停止

- `DHTServer::new()` 创建并绑定所需 Socket，失败直接返回 `Err`。
- `start()` 启动后台任务并等待取消，因此通常应在应用主任务中 await。
- `shutdown()` 可从 clone handle 调用，取消 DHT、crawl 和 Metadata 后台任务。
- 已 shutdown 的同一实例不能重新 start；需要重新构造 `DHTServer`。
- `on_error` 用于接收运行期协议/worker 错误；初始化错误仍通过 `Result` 返回。

## 运行时观测

不启用 `metrics` feature 也可以读取运行时快照：

```rust
let stats = server.runtime_stats();

let runtime = stats.snapshot();
println!(
    "nodes={} metadata={}/{} in_flight={}",
    runtime.node_pool_size,
    runtime.metadata_queue_depth,
    runtime.metadata_queue_max,
    runtime.metadata_in_flight,
);

let observability = stats.observability_snapshot();
println!(
    "udp rx={}B tx={}B fetch_p95={:?}ms",
    observability.udp_rx_bytes,
    observability.udp_tx_bytes,
    observability.fetch_duration_ms.percentile(0.95),
);
```

快照使用 relaxed atomic load，适合监控，不是跨字段事务视图。计数器是进程生命周期
累计值，调用方通过相邻快照差值计算 rate，并应处理进程重启导致的 counter reset。

固定桶：

| 直方图 | 边界/单位 |
|---|---|
| Metadata queue wait | `10/50/100/250/500/1000/2000/5000 ms` |
| Metadata fetch | `250/500/1000/2000/4000/6000/10000 ms` |
| Metadata payload size | `16/32/64/128/256/512/1024 KiB, 10 MiB` |

`counts[i]` 表示小于等于 `bounds[i]` 的非累计桶计数，`overflow` 表示超过最后边界的
数量。`percentile()` 返回桶上界；落入 overflow 时只能返回最后一个上界，因此它是
有界近似值，不是精确分位数。

高级快照类型从 crate 根导出；当前 prelude 只重导出 `DhtRuntimeStats` 和
`DhtRuntimeSnapshot`。

## Prometheus / metrics

启用 `metrics` 后，库通过 `metrics` facade 记录 counter、gauge 和 histogram。
应用必须在创建/启动 server 前安装全局 recorder。示例：

```rust
use metrics_exporter_prometheus::PrometheusBuilder;

PrometheusBuilder::new()
    .with_http_listener("127.0.0.1:9000".parse().unwrap())
    .install()
    .unwrap();
```

完整名称、标签和单位见 [docs/metrics.md](docs/metrics.md)。

## 应用层集成

本 crate 只提供 DHT、BEP-9 Metadata、回调和观测能力，不包含 Redis、Manticore、
HTTP 看板或 `P1`～`P17` 性能档位。同级的 `dht-crawler-node` 项目负责这些应用层策略，
并把档位转换成具体的 `DHTOptions`。开发两个项目时应保持下面的目录关系：

```text
workspace-parent/
├── dht-crawler/
└── dht-crawler-node/
```

## JNI

启用 `jni` feature 可构建 `cdylib`。Java 示例、线程模型和 JNI 配置字段见
[examples-jni/README.md](examples-jni/README.md)。Java `DHTOptions` 是 Rust 配置的
扁平化子集，未暴露的 Bootstrap、Target、Scheduler 和 Peer failure cache 字段使用
Rust 默认值。

## 从 0.1 迁移到 0.2

0.2 是 breaking release：

| 0.1 | 0.2 |
|---|---|
| `metadata_timeout` | `metadata.timeout_secs` |
| `max_metadata_queue_size` | `metadata.max_queue_size` |
| `max_metadata_worker_count` | `metadata.max_worker_count` |
| `node_queue_capacity` | `crawl.pool.capacity` |
| 旧 active/candidate frontier | 单所有者严格 FIFO pool + responsive ring |
| 无交付确认 | `on_torrent_with_ack` + `DeliveryRejected` |
| 粗粒度统计 | `runtime_stats()` 的两类原子快照和固定桶 |

`DHTOptions` 不提供旧字段兼容层，升级时必须修改构造代码。配置档位属于应用策略，
不在库内实现。

## 构建与验证

```bash
cargo fmt --all --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo doc --no-deps --all-features
cargo run --release --example dht_crawler_example
```

JNI：

```bash
cargo build --release --features jni
```

## 许可证

[MIT](LICENSE)
