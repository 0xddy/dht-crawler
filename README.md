# dht-crawler

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
dht-crawler = "0.0.3"
```

## 快速开始

### 基本使用

请参考 `examples/main.rs` 查看完整的使用示例。

### 编译示例

```bash
# 编译 Linux 版本（推荐使用 mimalloc 以获得更好的内存性能）
cargo build --release --target x86_64-unknown-linux-gnu --examples --features mimalloc

# 编译后的可执行文件位于：
# target/x86_64-unknown-linux-gnu/release/examples/dht_crawler_example
```

使用 mimalloc 可以显著降低内存占用（通常降低 10-30%）。

## 许可证

MIT License
