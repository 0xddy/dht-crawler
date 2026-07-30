# Changelog

本项目遵循语义化版本。0.2.1 是包含公开 API 变更的 breaking release。

## 0.2.1 - 2026-07-30

### Breaking changes

- 将 `DHTOptions` 的 Metadata 和 crawl 参数改为嵌套结构：`MetadataOptions`、
  `CrawlOptions`、`RateLimitOptions`、`PoolOptions`、`BootstrapOptions`、
  `TargetOptions`、`SchedulerOptions`。
- 删除旧的 `metadata_timeout`、`max_metadata_queue_size`、
  `max_metadata_worker_count`、`node_queue_capacity` 等扁平字段。
- 删除旧 active/candidate frontier 和 sharded queue 实现，改用单所有者严格 FIFO
  节点池、recent-probe set 与 responsive-node ring。
- Metadata 调度改为有界、按 InfoHash 去重、最多三个 Peer、60 秒 freshness TTL。

### Added

- 独立的主动爬取 QPS、新目标、节点替换、回复包/字节、单来源回复、总在途和子网在途限制。
- 根据 Metadata 队列压力自动降低实际 `find_node` QPS。
- Bootstrap 来源退避、低水位触发和响应节点快照。
- `on_torrent_with_ack`、`on_metadata_fetch_complete`、
  `MetadataFetchCompletionStatus` 和真实 Peer `attempts`。
- 按 `SocketAddr` 缓存 Metadata Peer 的 timeout/connect failure。
- `DhtRuntimeStats::snapshot()`、`observability_snapshot()` 和三组固定桶直方图。
- DHT、UDP、节点池、Metadata scheduler/fetcher 的低基数 Prometheus 指标。

### Changed

- Metadata timeout 现在覆盖连接、握手、传输、SHA1 和解析的完整 Peer 尝试。
- UDP ingress、crawl events 和 Metadata queues 全部有界，并暴露 drop/depth 指标。
- DHT 回复增加总包、总字节、单来源限流，以及 `ping`/`get_peers` 10% 保底预算。
- JNI `DHTOptions` 更新为 0.2 配置的扁平化子集；未映射字段继续采用 Rust 默认值。
