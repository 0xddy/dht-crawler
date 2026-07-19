# dht-crawler 指标参考

启用 Cargo feature `metrics` 后，库通过 `metrics` facade 记录以下指标。库不安装
recorder、不监听端口，也不依赖任何特定导出协议；Prometheus exporter 应由最终应用安装。

所有 `*_total` 都是进程生命周期累计 counter。Gauge 是当前值。Histogram 的记录值
使用下表标出的单位。

## UDP 与 KRPC

| 指标 | 类型 | 标签 | 单位/含义 |
|---|---|---|---|
| `dht_udp_bytes_received_total` | counter | — | Socket 接收字节 |
| `dht_udp_packets_received_total` | counter | `status=ok|dropped_size|dropped_magic|queue_full` | UDP ingress 结果 |
| `dht_udp_bytes_sent_total` | counter | — | 成功发送字节 |
| `dht_udp_packets_sent_total` | counter | `type=query|response` | 成功发送包 |
| `dht_udp_query_size_bytes` | histogram | — | find_node query 编码长度，bytes |
| `dht_messages_processed_total` | counter | `type=q|r|e|unknown` | 成功解析的 KRPC 消息类型 |
| `dht_messages_parse_error_total` | counter | — | bencode/KRPC 解析失败 |
| `dht_queries_total` | counter | `q=ping|find_node|get_peers|announce_peer|vote|other_or_invalid` | 入站查询类型 |
| `dht_udp_responses_dropped_total` | counter | `reason=rate_limit` | 最终未发送的限流回复 |
| `dht_udp_responses_priority_reserved_total` | counter | `query=ping|get_peers` | 使用 10% 保底预算的回复 |

`dht_udp_bytes_received_total` 包含后续被判定为 invalid/queue-full 的 Datagram；发送侧只在
`send_to` 成功后累计。

## 主动爬取与节点池

| 指标 | 类型 | 标签 | 含义 |
|---|---|---|---|
| `dht_node_pool_size` | gauge | — | 当前 FIFO 节点数 |
| `dht_node_pool_oldest_age_seconds` | gauge | — | FIFO 最老节点年龄 |
| `dht_node_pool_admissions_total` | counter | — | 新准入节点 |
| `dht_node_pool_replacements_total` | counter | — | 满池替换 |
| `dht_node_pool_dropped_total` | counter | `reason=duplicate|rate_limit|invalid` | 节点拒绝原因 |
| `dht_find_node_in_flight` | gauge | — | 当前在途 find_node |
| `dht_find_node_effective_rate_per_second` | gauge | — | Metadata 压力调整后的实际预算 |
| `dht_crawl_queries_sent_total` | counter | `kind=new|revisit|bootstrap` | 已交给 egress 的查询用途；发送失败另计 |
| `dht_find_node_responses_total` | counter | — | 与 pending transaction 匹配的回复 |
| `dht_find_node_response_unmatched_total` | counter | — | 无匹配 pending 的回复 |
| `dht_find_node_timeouts_total` | counter | — | pending 超时 |
| `dht_find_node_send_failures_total` | counter | — | UDP query 发送失败 |
| `dht_crawl_events_dropped_total` | counter | `kind=discovered|response` | 有界 actor channel 丢弃 |
| `dht_metadata_queue_pressure_ratio` | gauge | — | Metadata depth/capacity，范围 0..1 |

actor 每秒把内部增量 flush 到 counter，因此 exporter 看到的 counter 可能最多延迟约一秒。

## announce 与 Metadata ingress

| 指标 | 类型 | 标签 | 含义 |
|---|---|---|---|
| `dht_announce_peer_blocked_total` | counter | `reason=invalid_token|filtered` | announce 拒绝原因 |
| `dht_info_hashes_discovered_total` | counter | — | token/hash/filter 校验通过的 InfoHash |
| `dht_metadata_ingress_dropped_total` | counter | `reason=queue_full` | Hash ingress 满导致的丢弃 |

## Metadata scheduler

| 指标 | 类型 | 标签 | 单位/含义 |
|---|---|---|---|
| `dht_metadata_queue_depth` | gauge | — | Pending Hash 数 |
| `dht_metadata_in_flight` | gauge | — | 当前 job 数 |
| `dht_metadata_queue_events_total` | counter | `result=inserted|deduplicated|evicted_oldest|stale|expired` | 队列事件 |
| `dht_metadata_queue_wait_seconds` | histogram | — | Hash 从最近发现到首次分派的秒数 |
| `dht_metadata_jobs_dispatched_total` | counter | — | 分派 job 数 |
| `dht_metadata_jobs_completed_total` | counter | `result=accepted|fetch_failed|delivery_rejected|gate_rejected` | job 终态 |
| `dht_metadata_worker_join_error_total` | counter | — | worker task join 失败 |
| `dht_metadata_completion_callback_panics_total` | counter | — | 完成回调 panic |

## Metadata Peer 下载

| 指标 | 类型 | 标签 | 单位/含义 |
|---|---|---|---|
| `dht_metadata_fetch_attempts_total` | counter | — | 实际 Peer 尝试 |
| `dht_metadata_peer_attempts_total` | counter | — | 与 fetch attempts 相同的 Peer 尝试计数 |
| `dht_metadata_fetch_success_total` | counter | — | 成功下载和解析 |
| `dht_metadata_fetch_result_total` | counter | `result=success|failed|timeout` | Peer 尝试结果 |
| `dht_metadata_fetch_fail_total` | counter | `reason=timeout|send_error|size_limit|sha1_mismatch|parse_error` | 详细失败原因 |
| `dht_metadata_connection_result_total` | counter | `result=success|failed` | TCP/BitTorrent connect 结果 |
| `dht_metadata_handshake_result_total` | counter | `result=success|no_extension_support` | 扩展能力/最终校验结果 |
| `dht_metadata_fetch_duration_seconds` | histogram | — | 端到端 Peer 尝试秒数 |
| `dht_metadata_size_bytes` | histogram | — | 完整 bencoded info payload 字节数 |
| `dht_metadata_bytes_downloaded_total` | counter | — | 收到的 Metadata piece 数据字节数 |
| `dht_metadata_peer_failure_cache_hits_total` | counter | `reason=timeout|connect_failed` | 坏 Peer 缓存命中 |
| `dht_metadata_peer_failure_cache_inserts_total` | counter | `reason=timeout|connect_failed` | 坏 Peer 缓存写入 |
| `dht_metadata_peer_failure_cache_entries` | gauge | — | 当前缓存条目数 |

`dht_metadata_fetch_result_total{result="failed"}` 汇总非 timeout 的失败，不适合单独用于
分析具体原因；详细原因应结合 `fetch_fail`、connection 和 handshake 指标。

## 与原子快照的关系

`DhtRuntimeStats` 始终可用，与 `metrics` feature 无关：

- `snapshot()` 提供队列、节点池、crawl、Peer 和 UDP 运行状态。
- `observability_snapshot()` 提供 UDP 字节/包、入站查询分类、announce、节点准入、
  Metadata 失败分类、failure cache 分类和固定桶。

两套出口在同一事件点更新，但读取时都不是跨字段事务快照；短时间内可能相差一个并发
事件，Prometheus 的 crawl actor counter 还可能有最多约一秒 flush 延迟。
