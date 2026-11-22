# dht-crawler

[![Crates.io](https://img.shields.io/crates/v/dht-crawler.svg)](https://crates.io/crates/dht-crawler)
[![Documentation](https://docs.rs/dht-crawler/badge.svg)](https://docs.rs/dht-crawler)
[![License](https://img.shields.io/crates/l/dht-crawler.svg)](LICENSE)

Rust + Tokio 写的 BitTorrent DHT 爬虫库。监听 UDP 端口接入 DHT 网络，通过主动探路和被动监听捞出 InfoHash，再并发连接 Peer 拉取并校验种子元数据（名称、文件列表、大小等）。

## 特点

- **主动 + 被动找种子**：覆盖被动 `announce_peer` / `get_peers`、主动 `find_node` 爬网、BEP-51 `sample_infohashes` 采样，以及缺 Peer 时的主动 `get_peers` 补人。
- **轻量协议栈**：内置 BEP-5 / 51 / 3 / 10 / 9 与 bencode 实现，不引入 `reqwest` / `rustls` 等重型依赖。
- **全链路有界**：节点池、去重队列、下载并发、UDP 速率、失败缓存、重试历史均有上限，长跑不爆内存。
- **多 Peer 竞速**：同一 InfoHash 并发尝试多个 Peer，首个 SHA1 校验通过即取消其余连接。
- **支持 IPv4 / IPv6 / 双栈**，内置无锁运行统计，可选接 `metrics`。

## 快速开始

```bash
cargo add dht-crawler
cargo add tokio --features rt-multi-thread,macros,signal
```

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

    server.on_torrent(|t| {
        println!("{}  {}  ({})", t.info_hash, t.name, t.format_size());
    });
    server.on_error(|e| eprintln!("DHT error: {e}"));

    let shutdown = server.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown.shutdown();
        }
    });

    server.start().await
}
```

运行仓库自带示例：

```bash
cargo run --release --example dht_crawler_example
```

> `DHTServer::new()` 会立即绑定 UDP 端口；`shutdown()` 停止后不可重用，重启需重新创建实例。

## 回调接入

同类回调重复注册时，新的覆盖旧的。

| 回调 | 触发时机 | 用途 |
|---|---|---|
| `filter(\|hash\| -> bool)` | 入队前（同步，勿阻塞） | 查内存去重，返回 `false` 直接丢弃 |
| `on_metadata_fetch(\|hash\| async -> bool)` | 联网抓取前 | 查库去重，返回 `false` 放弃（不触发完成回调） |
| `on_metadata_admission(\|hash\| async -> MetadataAdmission)` | 联网抓取前 | `Fetch` 抓取 / `Skip` 跳过 / `RetryAfter(d)` 保留 Peer 延后再查（适合分布式锁） |
| `on_torrent(\|torrent\|)` | SHA1 校验通过后 | 接收已校验的 `TorrentInfo` |
| `on_torrent_with_ack(\|torrent\| -> bool)` | SHA1 校验通过后 | 返回 `false`（或 panic）视为下游拒收 |
| `on_metadata_fetch_complete(\|done\|)` | 已准入任务结束时（必触发且仅一次） | 释放锁 / 记冷却；状态为 `Accepted` / `FetchFailed` / `DeliveryRejected` |
| `on_error(\|err\|)` | 运行期异常 | 记录错误，不中断主循环 |

```rust
use std::time::Duration;

server.filter(|hash| !seen_in_memory(hash));

server.on_metadata_admission(|hash| async move {
    match claim_in_redis(&hash).await {
        Claim::Ok => MetadataAdmission::Fetch,
        Claim::Exists => MetadataAdmission::Skip,
        Claim::Busy => MetadataAdmission::RetryAfter(Duration::from_secs(30)),
    }
});

server.on_torrent_with_ack(|t| tx.try_send(t).is_ok());

server.on_metadata_fetch_complete(|done| {
    // done.attempts 只统计真实联网尝试次数，不含失败缓存跳过
    release_lock(&done.info_hash, done.status, done.attempts);
});
```

## 配置要点

从 `DHTOptions::default()` 起步，按机器规格调整即可（完整字段见 [docs.rs](https://docs.rs/dht-crawler)）：

```rust
let options = DHTOptions {
    port: 6881,
    netmode: NetMode::Ipv4Only,
    metadata: MetadataOptions {
        timeout_secs: 5,          // 单次 Peer 抓取总超时（秒）
        connect_timeout_ms: 2000, // 独立 TCP 建连超时（毫秒，0 默认按 2000 算）
        max_queue_size: 20_000,
        max_worker_count: 512,
        ..Default::default()
    },
    ..Default::default()
};
```

关键行为与调参提示：

- **默认网络模式**：`NetMode::Ipv4Only`；有公网 IPv6 再切 `DualStack`（分别绑定 v4/v6 socket）。
- **高并发 FD 上限**：`max_worker_count` 调到上千时，Linux 需同步提高 `ulimit -n` / `LimitNOFILE`（如 `262144`）。
- **单 Hash 尝试与冷却**：最多扫 `64` 个候选、真实联网最多 `12` 次；TCP 连不上按 `IP:Port` 冷却，建连后握手/Reject/协议失败按 `(InfoHash, IP:Port)` 冷却，不误伤同节点其他种子；整任务失败支持 `retry_*` 指数退避重试。
- **按需开关主动功能**：`peer_lookup.max_lookups_per_second = 0` 关闭主动 `get_peers`；`discovery.sample_queries_per_second = 0` 关闭 BEP-51 采样；`discovery.enabled = false` 仅保留 `announce_peer`。
- **活跃节点重访间隔**：`crawl.pool.revisit_interval_secs` 默认 `0`，想降低重复探测可设为 `30` 秒左右。

## 监控与排障

**内存快照（默认可用）**：通过 `server.runtime_stats()` 读取：

```rust
let s = server.runtime_stats().snapshot();
println!(
    "nodes={} queue={}/{} workers={} ok={} fail={}",
    s.node_pool_size, s.metadata_queue_depth, s.metadata_queue_max,
    s.metadata_in_flight, s.metadata_peer_succeeded, s.metadata_peer_failed,
);
```

- 更细的排障数据看 `observability_snapshot()`：包含分阶段超时（`metadata_timeout_stages`；其中 `connect_handshake` 为 `tcp_connect + base_handshake` 兼容汇总，统计时勿重复相加）、连接错误细分（`metadata_connection_errors`）、扩展阶段断连/协议错误（`metadata_failure_peer_closed` / `metadata_failure_protocol`）及延迟/体积直方图。
- **对接 Prometheus**：启用 `features = ["metrics"]` 通过 `metrics` facade 打点（库不启动 HTTP 服务，需在主程序自行安装 exporter），指标清单见 [docs/metrics.md](docs/metrics.md)。

## Cargo Features

默认均关闭：

| Feature | 说明 |
|---|---|
| `metrics` | 启用 `metrics` 埋点 |
| `jni` | 编译 Java/Kotlin JNI 接口（`cdylib`），JVM 封装见 [`dht-crawler-java`](https://github.com/0xddy/dht-crawler-java) |
| `mimalloc` | 仅作用于 `dht_crawler_example` 示例，不影响作为库引入时的全局分配器 |

## 开发验证

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## 许可证

[MIT](LICENSE)
