# dht-crawler-jni Java 示例

本目录是一个 Gradle 管理的 Java 项目，演示如何通过 JNI 使用 `dht-crawler` Rust 库。

## 项目结构

```
examples-jni/
├── build.gradle                        # Gradle 构建脚本
├── settings.gradle
└── src/main/java/cn/lmcw/dht/
    ├── model/
    │   ├── TorrentInfo.java            # 与 Rust TorrentInfo 一一对应
    │   ├── FileInfo.java               # 与 Rust FileInfo 一一对应
    │   └── DHTOptions.java             # 与 Rust DHTOptions 一一对应
    ├── DhtListener.java                # 事件回调接口
    ├── DhtCrawlerJni.java              # JNI 绑定类（native 方法声明）
    └── DhtCrawlerExample.java          # 可运行示例
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

# Windows（将 dht_crawler.dll 放在同目录）
java -Djava.library.path=. -jar dht-crawler-jni-example-<version>.jar
```

### 方式二：从源码编译并运行

#### 1. 编译 Rust JNI 动态库

在仓库**根目录**执行：

```bash
# 本机（Linux 产出 libdht_crawler.so，Windows 产出 dht_crawler.dll，macOS 产出 libdht_crawler.dylib）
cargo build --release --features jni
```

产物路径：`target/release/`

#### 2. 运行 Java 示例

在本目录（`examples-jni/`）执行：

```bash
# 使用默认库路径（../target/release）
gradle run

# 自定义库路径
gradle run -Plib.path=/path/to/your/lib
```

#### 3. 构建 fat JAR

```bash
gradle shadowJar
# 产物：build/libs/dht-crawler-jni-example-<version>.jar
```

## 在自己的项目中集成

1. 将 `src/main/java/cn/lmcw/dht/` 下的文件复制到你的项目。
2. 将对应平台的 so/dll/dylib 放入 `java.library.path` 可访问的目录。
3. 确保 JVM 启动时加了 `-Djava.library.path=<路径>`。

## JNI 生命周期

```java
DHTOptions options = new DHTOptions().setPort(6881);
long handle = DhtCrawlerJni.createServer(options, listener);
DhtCrawlerJni.startServer(handle);
// ... 运行 ...
DhtCrawlerJni.stopServer(handle);
DhtCrawlerJni.destroyServer(handle); // 必须调用，释放 Rust 资源
```

## 注意事项

- `destroyServer` 必须在不再使用后调用，否则 Rust 侧的 tokio runtime 和连接不会释放。
- 回调方法（`onTorrent`、`onError`）在 Rust 的 tokio 工作线程中触发，请确保实现是线程安全的。
- 同一个句柄不要在多个线程中并发调用 `destroyServer`。
