# dht-crawler-jni Java 示例

本目录是一个 Gradle 管理的 Java 项目，演示如何通过 JNI 使用 `dht-crawler` Rust 库。

## 项目结构

```
examples-jni/
├── build.gradle
├── settings.gradle
└── src/main/java/cn/lmcw/dht/
    ├── model/          # DHTOptions, TorrentInfo, FileInfo
    ├── DhtCrawler.java      # 面向对象入口（推荐）
    ├── DhtCrawlerJni.java # 包内 native 绑定
    ├── DhtListener.java
    └── DhtCrawlerExample.java
```

## 快速开始

### 方式一：直接下载 Release 中的 JAR 运行（推荐）

从 GitHub Release 页面下载两个文件：

1. `dht-crawler-jni-example-<version>.jar` — 平台无关的 fat JAR
2. 对应平台的 JNI 动态库 zip（如 `dht_crawler_jni-<version>-x86_64-unknown-linux-gnu.zip`），解压得到 `libdht_crawler.so` / `dht_crawler.dll` / `libdht_crawler.dylib`

将 JAR 和动态库放到同一目录，然后执行：

```bash
# Linux / macOS
java -Djava.library.path=. -jar dht-crawler-jni-example-<version>.jar

# Windows（PowerShell/CMD 需对 -D 参数加引号，否则会报找不到主类）
java "-Djava.library.path=." -jar dht-crawler-jni-example-<version>.jar
```

### 方式二：从源码编译并运行

#### 1. 编译 Rust JNI 动态库

在仓库**根目录**执行：

```bash
cargo build --release --features jni
```

产物路径：`target/release/`

#### 2. 运行 Java 示例

在本目录（`examples-jni/`）执行：

```bash
gradle run
gradle run -Plib.path=/path/to/your/lib
```

#### 3. 构建 fat JAR

```bash
gradle shadowJar
```

## 在自己的项目中集成

1. 复制 `cn/lmcw/dht/` 下源码（含 `model/`、`DhtCrawler`、`DhtCrawlerJni`、`DhtListener`）。
2. 将对应平台的 so/dll/dylib 放入 `java.library.path`。

## API（面向对象）

```java
DhtCrawler crawler = DhtCrawler.createServer(options, listener);
crawler.start();
// ...
crawler.stop();   // 或 try-with-resources
```

- **`DhtCrawler.createServer(options, listener)`**：创建会话（未启动 DHT；Java 不能用方法名 `new`，故不用 `open`）。
- **`start()`**：后台启动 DHT，非阻塞；同一会话多次 `start()` 仅首次生效。
- **`stop()` / `close()`**：停止并释放 Rust 资源，幂等。
- **`getNodePoolSize()`**：routing table 节点数。

## 注意事项

- 回调在 Rust 工作线程触发，实现需线程安全。
- `onMetadataFetch` 在阻塞线程池调用，宜快速返回。
