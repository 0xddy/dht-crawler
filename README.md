# dht-crawler

[![Crates.io](https://img.shields.io/crates/v/dht-crawler.svg)](https://crates.io/crates/dht-crawler)
[![Documentation](https://docs.rs/dht-crawler/badge.svg)](https://docs.rs/dht-crawler)
[![License](https://img.shields.io/crates/l/dht-crawler.svg)](https://github.com/yourusername/dht-crawler/blob/master/LICENSE)

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
dht-crawler = "0.0.5"
```

或者查看 [crates.io](https://crates.io/crates/dht-crawler) 获取最新版本号。

### 启用可选 Features

如果需要使用 Prometheus 指标支持，可以启用 `metrics` feature：

```toml
[dependencies]
dht-crawler = { version = "0.0.5", features = ["metrics"] }
```

## Features

本库支持以下可选特性：

### `mimalloc` - 高性能内存分配器

使用 `mimalloc` 作为全局内存分配器，可以显著降低内存占用（通常降低 10-30%），特别适合长时间运行的高并发场景。

### `metrics` - Prometheus 指标支持

启用 `metrics` feature 后，库会通过 `metrics` crate 暴露统计指标，可以与 Prometheus 等监控系统集成。

**注意**：
- `mimalloc` 和 `metrics-exporter-prometheus` 仅在编译 example 时可用（位于 `dev-dependencies`）
- 使用 example 时需要显式启用对应的 features
- 库本身可以使用 `metrics` feature（通过 `[dependencies]` 中的可选依赖）

## 快速开始

### 基本使用

请参考 `examples/main.rs` 查看完整的使用示例。

### 编译示例

#### 基本编译（不启用任何 features）

```bash
# Debug 模式
cargo build --example dht_crawler_example

# Release 模式
cargo build --release --example dht_crawler_example
```

#### 启用 mimalloc（推荐）

```bash
# Debug 模式
cargo build --example dht_crawler_example --features mimalloc

# Release 模式（推荐用于生产环境）
cargo build --release --example dht_crawler_example --features mimalloc
```

#### 启用 metrics（Prometheus 指标导出）

```bash
# 启用 metrics feature
cargo build --example dht_crawler_example --features metrics

# Release 模式
cargo build --release --example dht_crawler_example --features metrics
```

#### 同时启用 mimalloc 和 metrics

```bash
# Debug 模式
cargo build --example dht_crawler_example --features mimalloc,metrics

# Release 模式（推荐）
cargo build --release --example dht_crawler_example --features mimalloc,metrics
```

#### 交叉编译（Linux）

```bash
# 编译 Linux 版本（推荐使用 mimalloc 以获得更好的内存性能）
cargo build --release --target x86_64-unknown-linux-gnu --examples --features mimalloc,metrics

# 编译后的可执行文件位于：
# target/x86_64-unknown-linux-gnu/release/examples/dht_crawler_example
```

### 运行示例

#### 基本运行（不使用任何 features）

```bash
cargo run --example dht_crawler_example
```

#### 启用 mimalloc 运行

```bash
cargo run --example dht_crawler_example --features mimalloc
```

#### 启用 metrics 运行

启用 metrics 后，Prometheus metrics 导出器会在 `http://localhost:9000/metrics` 启动。

```bash
cargo run --example dht_crawler_example --features metrics
```

然后在浏览器或使用 curl 访问：
```bash
curl http://localhost:9000/metrics
```

#### 同时启用 mimalloc 和 metrics（推荐）

```bash
cargo run --example dht_crawler_example --features mimalloc,metrics
```

### 在项目中使用 Features

#### 使用库本身（不带任何 features）

```toml
[dependencies]
dht-crawler = "0.0.4"
```

#### 使用库并启用 metrics feature

```toml
[dependencies]
dht-crawler = { version = "0.0.4", features = ["metrics"] }
```

这样可以在你的代码中使用库暴露的 metrics 指标。

**注意**：`mimalloc` feature 仅用于 example，库本身不依赖 mimalloc。

## 许可证

MIT License
