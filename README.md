# dht-crawler

[![Crates.io](https://img.shields.io/crates/v/dht-crawler.svg)](https://crates.io/crates/dht-crawler)
[![Documentation](https://docs.rs/dht-crawler/badge.svg)](https://docs.rs/dht-crawler)
[![License](https://img.shields.io/crates/l/dht-crawler.svg)](LICENSE)

一个用 Rust + Tokio 写的 BitTorrent DHT 爬虫库。

说白了就是：**你给它一个 UDP 端口，它自己混进全网 DHT 里到处“打招呼”，把别人正在下载或广播的磁力链接（InfoHash）捞出来，再去连对应的 Peer 把种子里的文件名、目录、大小这些元数据（Metadata）拉回来、做完 SHA1 校验后交给你。**

## 它有什么特点

- **主动 + 被动一起找种子**：不只是被动蹲别人发来的 `announce_peer` 和 `get_peers`，还会主动发 `find_node` 扩充节点池、发 BEP-51 `sample_infohashes` 采样热门节点；拿到只有 InfoHash、没有现成 Peer 的线索时，会自动发 `get_peers` 去找能下元数据的 Peer。
- **轻量、纯原生协议栈**：内置 BEP-5（DHT）、BEP-51（采样）、BEP-3/10/9（BitTorrent 握手与 `ut_metadata` 下载）和 bencode 编解码，不拖家带口引入 `reqwest` / `rustls` 这类重型依赖。
- **所有队列和并发都有上限，跑久了不爆内存**：节点池、待抓队列、下载并发、UDP 发包/回包速率、失败缓存、重试历史全都有界，流量超载时按预算丢弃或降速，不会无限堆内存。
- **支持 IPv4、IPv6 和双栈**。
- **开箱即用的运行状态统计**：不搭 Prometheus 也能通过 `runtime_stats()` 直接看队列深度、发包速率和各种失败原因细分；需要对接监控系统时再开 `metrics` feature。

## 它是怎么跑起来的

```text
全网 DHT (UDP)
  ├── 主动探路 (find_node) ───────────────────▶ 节点池 (扩充好友 + 混脸熟)
  ├── 被动监听 (announce_peer / get_peers) ─┐
  └── 主动采样 (BEP-51 sample_infohashes)  ─┼──▶ 同步过滤 (filter)
                                            │         │
                                            ▼         ▼
                                       待抓队列 (按 InfoHash 去重)
                                                      │
                                            异步准入 (查库 / Redis 抢锁)
                                                      │
                                       多 Peer 并发竞速拉元数据 (BEP-9)
                                        (缺 Peer 时自动发 get_peers 补人)
                                                      │
                                       SHA1 校验通过 ──▶ 回调交给你 (TorrentInfo)
```

同一个种子经常同时有好几个 Peer 线索，调度器会让多个候选 Peer 并发竞速去下——**谁先下完且 SHA1 校验通过，立刻取消剩下的连接**，把带宽和句柄留给下一个种子。

## 5 分钟跑起来

### 安装

```bash
cargo add dht-crawler
cargo add tokio --features rt-multi-thread,macros,signal
```

或者直接写进 `Cargo.toml`：

```toml
[dependencies]
dht-crawler = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

### 最小示例

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

    // 抓到并校验通过的种子都会走到这里
    server.on_torrent(|torrent| {
        println!(
            "{}  {}  ({})",
            torrent.info_hash,
            torrent.name,
            torrent.format_size()
        );
    });

    // 运行期错误（比如个别发包失败），不会让整个进程 panic
    server.on_error(|error| {
        eprintln!("DHT 运行出错: {error}");
    });

    // 收到 Ctrl-C 时优雅停机
    let shutdown = server.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown.shutdown();
        }
    });

    // 阻塞运行，直到调用 shutdown()
    server.start().await
}
```

想直接体验效果，可以跑仓库里自带的完整示例：

```bash
cargo run --release --example dht_crawler_example
```

> 小提醒：`DHTServer::new()` 会立刻绑定你配的 UDP 端口。`shutdown()` 停掉之后，这个实例不能再重新 `start()`，要重启请重新 `new` 一个。

## 怎么和你的业务接起来

实际跑爬虫时，一般要和数据库、Redis 或消息队列配合，避免重复抓取或者下游写不过来。`DHTServer` 提供了下面几个回调（同一类回调重复注册时，新的会覆盖旧的）：

### 1. 进门先拦一下：`filter`（同步）

在 InfoHash 进入队列之前执行，适合查内存布隆过滤器、本地 LRU 缓存这种**极快且不阻塞**的判断。返回 `false` 直接丢掉，不占队列名额：

```rust
server.filter(|info_hash| !seen_in_memory(info_hash));
```

### 2. 真正联网抓之前再确认：`on_metadata_fetch` / `on_metadata_admission`（异步）

下载元数据要建 TCP 连接，比较耗资源。如果你想在动手前去查数据库或 Redis 抢个分布式锁，用异步准入回调：

- **简单场景（要或不要）**——用 `on_metadata_fetch`：

```rust
server.on_metadata_fetch(|info_hash| async move {
    !exists_in_db(&info_hash).await
});
```

返回 `false` 会直接放弃这个种子，不建连接，也不会触发后面的完成回调。不注册准入回调时默认全部放行。

- **带锁/冷却的进阶场景**——用 `on_metadata_admission`：

```rust
use std::time::Duration;

server.on_metadata_admission(|info_hash| async move {
    match try_claim_in_redis(&info_hash).await {
        ClaimResult::Acquired => MetadataAdmission::Fetch,
        ClaimResult::AlreadyExists => MetadataAdmission::Skip,
        // 别的节点正在抓，先别扔掉已发现的 Peer，30 秒后再问一次
        ClaimResult::Busy => MetadataAdmission::RetryAfter(Duration::from_secs(30)),
    }
});
```

### 3. 收种子并确认下游接没接住：`on_torrent` / `on_torrent_with_ack`

交到你手里的 `TorrentInfo` 已经做过 SHA1 校验，里面有 `info_hash`、`magnet_link`、`name`、`total_size`、`files`、`piece_length`、`peers` 和 `timestamp`。

- 不关心下游有没有背压，直接用 `on_torrent`。
- 如果你是把结果塞进一个有界 channel / 写队列，用 `on_torrent_with_ack` 返回 `true`（收下了）或 `false`（塞不进去拒收；回调里 panic 也会被捕获并当成拒收）：

```rust
server.on_torrent_with_ack(|torrent| output_tx.try_send(torrent).is_ok());
```

### 4. 任务收尾通知：`on_metadata_fetch_complete`

只要一个种子通过了准入检查，不管最后成没成，**一定会且只会**回调一次，方便你释放 Redis 锁或记录失败冷却：

```rust
server.on_metadata_fetch_complete(|done| {
    println!(
        "{}: {:?}, 实际尝试 Peer 数={}",
        done.info_hash, done.status, done.attempts
    );
});
```

`done.status` 只有三种结果：

| 状态 | 什么意思 |
|---|---|
| `Accepted` | 元数据下完、校验通过，而且你的回调收下了 |
| `FetchFailed` | 能试的 Peer 全都失败了 |
| `DeliveryRejected` | 元数据明明下成功了，但你的 `on_torrent_with_ack` 返回了 `false`（或 panic） |

> `done.attempts` 只算真正发出网络请求的次数，因为命中失败缓存跳过的 Peer 不会算进来。被准入回调拒掉的任务不会触发这个回调。

## 常用配置怎么调

平时从 `DHTOptions::default()` 起步，按机器带宽、CPU 和下游吞吐改几个核心参数就行：

```rust
let options = DHTOptions {
    port: 6881,
    netmode: NetMode::Ipv4Only,
    hash_queue_capacity: 20_000,
    metadata: MetadataOptions {
        timeout_secs: 5,          // 单个 Peer 尝试的总超时（秒）
        connect_timeout_ms: 2000, // 单独限制 TCP 建连超时（毫秒），防止死 IP 拖死整次尝试
        max_queue_size: 20_000,   // 待抓去重队列上限
        max_worker_count: 512,    // 最多同时跑多少个元数据抓取任务
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

所有配置按职责拆成了几组（完整字段和默认值看 [docs.rs 文档](https://docs.rs/dht-crawler)）：

| 配置结构体 | 管什么的 |
|---|---|
| `DHTOptions` | 监听端口、IPv4/IPv6 模式、入口 Hash 队列大小 |
| `MetadataOptions` | 元数据下载总超时、TCP 建连超时、队列/并发上限、失败 Peer 缓存、失败退避重试 |
| `PeerLookupOptions` | 主动发 `get_peers` 找 Peer 的速率、并发与超时 |
| `DiscoveryOptions` | 被动 `get_peers` 捞 Hash 和主动 BEP-51 采样的开关与速率预算 |
| `CrawlOptions.rate_limit` | 主动 `find_node` 发包速率、在途上限、UDP 回包限流与背压降速比例 |
| `CrawlOptions.pool` | 节点池容量、近期探测去重 TTL、活跃节点缓存与重访间隔 |
| `CrawlOptions.bootstrap` | 引导节点列表与失败退避 |
| `CrawlOptions.target` | 随机游走 / 稀疏桶探测比例，以及是否借用目标前缀构造邻近节点 ID |
| `CrawlOptions.scheduler` | 内部事件通道容量、批处理大小与路由快照刷新频率 |

### 几个容易踩坑的细节

- **默认只开 IPv4**：`DHTOptions::default()` 是 `NetMode::Ipv4Only`。服务器确定有公网 IPv6 再切 `NetMode::DualStack`（会分别绑定 v4 和 v6 两个 UDP socket）。
- **高并发别忘了提文件描述符上限**：把 `metadata.max_worker_count` 调到上千时，Linux 下请同步把 `ulimit -n` / systemd `LimitNOFILE` 拉高（例如 262144），不然大量并发 TCP 连接会被系统 `EMFILE` 拦下来。
- **独立 TCP 建连超时**：`metadata.connect_timeout_ms` 默认 `2000` 毫秒（填 `0` 也按 `2000` 毫秒算，且不会超过 `timeout_secs`）。这样遇到彻底不通的死 IP 能快速止损，把时间留给后面的候选 Peer。
- **防反复撞墙机制**：
  - 同一个 InfoHash 最多扫描 `64` 个候选 Peer、真正发网络请求最多 `12` 次。
  - TCP 根本连不上的地址会按 **IP:Port** 短期冷却；已经连上但握手/拉元数据失败或被对方 Reject 的，只按 **(InfoHash, IP:Port)** 冷却，不会误伤这个 Peer 上的其他种子。
  - 全部 Peer 都失败的任务还会按 `retry_*` 参数做指数退避重试，重试前同样会重新走你的准入回调。
- **不想开某些主动行为可以单独关**：
  - `peer_lookup.max_lookups_per_second = 0`：关掉主动 `get_peers` 补 Peer；
  - `discovery.sample_queries_per_second = 0`：关掉 BEP-51 主动采样（仍保留被动 `get_peers` 捞 Hash）；
  - `discovery.enabled = false`：把上面两项一起关掉，只靠 `announce_peer`；
  - `crawl.pool.revisit_interval_secs`：默认 `0`（活跃节点可随时重访），如果想减少对同一个节点的重复探测，可以试着设成 `30` 秒左右。

## 怎么看它在干嘛、哪里卡住了

### 方式一：直接读内存统计（默认可用，零额外依赖）

随时调用 `server.runtime_stats()` 就能拿到实时数据，自己打日志或做 HTTP 面板都很方便：

```rust
let stats = server.runtime_stats();
let s = stats.snapshot();

println!(
    "节点池={} | 待抓队列={}/{} | 抓取中={} | 成功={} | 失败={}",
    s.node_pool_size,
    s.metadata_queue_depth,
    s.metadata_queue_max,
    s.metadata_in_flight,
    s.metadata_peer_succeeded,
    s.metadata_peer_failed,
);
```

如果发现抓得慢、成功率掉下来，可以读更详细的 `stats.observability_snapshot()` 来排障：

- **超时到底卡在哪一步**：看 `metadata_timeout_stages`，细分成 `tcp_connect`（TCP 建连）、`base_handshake`（基础握手）、`extension_handshake_send/receive`（扩展握手）、`piece_request_send` / `piece_receive`（拉分片）和 `validation`（校验）。
  - *注意*：`connect_handshake` 是为兼容旧监控保留的 `tcp_connect + base_handshake` 合计值，画图统计时别再把它们加起来重复算。
- **连不上到底是什么错**：看 `metadata_connection_errors`，区分 `refused`、`reset`、`closed`、`timeout`、`invalid_handshake`、`info_hash_mismatch` 等。
- **元数据下载失败原因**：细分了 `metadata_failure_no_extension`（对方不支持扩展协议）、`metadata_failure_reject`（对方拒绝给）、`metadata_failure_peer_closed`（扩展阶段被对方断开）、`metadata_failure_protocol`（协议数据异常）、`metadata_failure_size_limit`（超过 10 MiB 上限）、`metadata_failure_sha1`（校验失败）、`metadata_failure_parse`（解析失败）等。
- **耗时与体积分布**：自带固定桶直方图 `queue_wait_ms`、`fetch_duration_ms`、`metadata_size_bytes`。

> 快照是面向监控的无锁原子读取，字段之间不做全局锁快照，高并发下相邻字段有极小的时间差属于正常现象。

### 方式二：对接 Prometheus 等监控（可选 `metrics` feature）

```toml
[dependencies]
dht-crawler = { version = "0.2", features = ["metrics"] }
```

开启后库会通过 [`metrics`](https://crates.io/crates/metrics) facade 打点，但**不会帮你装 exporter，也不会偷偷监听任何 HTTP 端口**——你需要在自己的主程序里安装 `metrics-exporter-prometheus` 等 recorder。完整指标列表见 [docs/metrics.md](docs/metrics.md)。

## Cargo Features

默认不开启任何 feature。

| Feature | 作用 |
|---|---|
| `metrics` | 通过 `metrics` facade 输出监控指标 |
| `jni` | 编译 Java/Kotlin JNI 接口（`cdylib`），配套 JVM 封装见 [`dht-crawler-java`](https://github.com/0xddy/dht-crawler-java) |
| `mimalloc` | 仅用于仓库自带的 `dht_crawler_example` 示例启用 mimalloc 分配器；作为库被依赖时不会改动你的全局分配器 |

## 项目结构一览

想看源码或者二开，直接按职责找对应目录就行：

- `src/server/`：对外主入口 `DHTServer`、UDP 收包与回包限流
- `src/crawl_engine.rs` & `src/node_pool/`：主动 `find_node` 爬网引擎、FIFO 节点池与活跃节点维护
- `src/discovery.rs`：被动 `get_peers` 捞 Hash + 主动 BEP-51 `sample_infohashes` 采样
- `src/peer_lookup.rs`：针对缺 Peer 的 InfoHash 主动发起 `get_peers` 查找
- `src/scheduler/`：待抓队列去重、异步准入、多 Peer 竞速与指数退避重试
- `src/metadata/`：纯原生 bencode、BEP-3/10/9 Peer 协议实现、SHA1 校验解析与失败缓存
- `src/runtime_stats/`：无锁运行时统计、直方图与产出追踪

## 本地开发与验证

```bash
cargo fmt --all --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo doc --no-deps --all-features
```

## 许可证

[MIT](LICENSE)
