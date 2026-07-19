# dht-crawler JNI Java 示例

Gradle 项目，演示通过 JNI 调用 `dht-crawler`（需启用 Cargo feature `jni`）。

## 项目结构

```
examples-jni/
├── build.gradle
├── settings.gradle
└── src/main/java/cn/lmcw/dht/
    ├── model/              # DHTOptions, TorrentInfo, FileInfo
    ├── DhtCrawler.java     # 面向对象入口（推荐）
    ├── DhtCrawlerJni.java  # native 方法声明
    ├── DhtListener.java    # 回调接口
    └── DhtCrawlerExample.java
```

Rust JNI 实现位于 **`dht-crawler/jni/`**（`lib` crate-type 含 `cdylib`）。

## 编译 native 库

在 **`dht-crawler`** 目录（本 README 的上一级）执行：

```bash
cargo build --release --features jni
```

产物路径（因平台而异）：

- Linux: `target/release/libdht_crawler.so`
- Windows: `target/release/dht_crawler.dll`
- macOS: `target/release/libdht_crawler.dylib`

## 运行示例

### 方式一：Release 预编译包（推荐）

下载 Release 中的 fat JAR 与对应平台 native 库 zip，放在同一目录：

```bash
# Linux / macOS
java -Djava.library.path=. -jar dht-crawler-jni-example-<version>.jar

# Windows
java "-Djava.library.path=." -jar dht-crawler-jni-example-<version>.jar
```

### 方式二：源码 + Gradle

```bash
cd examples-jni
gradle run
# 指定 native 库目录（默认为 ../target/release）
gradle run -Plib.path=/path/to/lib
```

构建 fat JAR：

```bash
gradle shadowJar
```

## 集成到自己的 Java 项目

1. 复制 `cn/lmcw/dht/` 包（含 `model/`、`DhtCrawler`、`DhtCrawlerJni`、`DhtListener`）。
2. 将对应平台的 native 库加入 `java.library.path`。
3. 使用与示例相同的 `dht-crawler` JNI 版本构建 `cdylib`。

## API（面向对象）

```java
DhtCrawler crawler = DhtCrawler.createServer(options, listener);
crawler.start();   // 后台启动，不阻塞调用线程
// ...
crawler.stop();    // 或 try-with-resources
```

| 方法 | 说明 |
|------|------|
| `createServer(options, listener)` | 创建 `ServerHandle`（含独立 tokio `Runtime` + `DHTServer`） |
| `start()` | 在 runtime 内 spawn `server.start()`；同一会话多次调用仅首次生效 |
| `stop()` / `close()` | `shutdown()` 后在后台线程 drop Runtime，避免阻塞 JNI 线程 |
| `getNodePoolSize()` | 节点池大小（`DHTServer::get_node_pool_size`） |

Native 导出类：`cn.lmcw.dht.DhtCrawlerJni`（`createServer` / `startServer` / `stopServer` / `getNodePoolSize`）。

## 回调与线程

- `onTorrent` / `onError`：在 Rust 工作线程触发，Java 实现须线程安全。
- `onMetadataFetch`：在阻塞线程池中调用，应尽快返回 boolean。

行为与 Rust 库一致：InfoHash 来自 **`announce_peer`**；`start()` 在 Rust 侧仍阻塞至 `shutdown()`，JNI 通过单独 runtime + `spawn` 避免卡住 Java 主流程。

当前 JNI listener 暴露 `onTorrent`、`onMetadataFetch` 和 `onError`，不暴露 Rust
`on_torrent_with_ack` / `on_metadata_fetch_complete`。因此 Java `onTorrent` 返回后始终按
`Accepted` 处理；需要交付确认和最终状态的应用应扩展 JNI callback contract。

## JNI 配置映射

Java `DHTOptions` 是 Rust 配置的扁平化子集，不是全部 Rust 字段的一一镜像：

| Java 字段组 | Rust 目标 |
|---|---|
| port / netMode / hashQueueCapacity | `DHTOptions` 顶层 |
| metadataTimeout / maxMetadataQueueSize / maxMetadataWorkerCount | `metadata.*` |
| poolCapacity / recentProbeTtlSeconds / responsive* / poolLowWatermark | `crawl.pool.*` |
| findNode* / requestTimeout* / response* / pressure / replacements / subnet | `crawl.rate_limit.*` |

以下配置未通过当前 JNI 暴露，使用 Rust `Default`：

- `metadata.peer_failure_cache_capacity`、`metadata.peer_failure_ttl_secs`
- `crawl.bootstrap.*`
- `crawl.target.*`
- `crawl.scheduler.*`

Java 字段默认值与当前 Rust 0.2 默认值保持一致；传入 `null` options 时直接使用完整的
`DHTOptions::default()`。

## 注意事项

- Java 11+（见 `build.gradle`）。
- Java/Rust 版本必须一致，避免 JNI 按字段名和签名读取时失败。
- 停止后勿再使用同一 `long` 句柄。
