package cn.lmcw.dht;

import cn.lmcw.dht.model.TorrentInfo;

/**
 * DHT 爬虫事件回调接口。
 * <p>实现此接口并传入 {@link DhtCrawlerJni#createServer} 即可接收事件通知。
 * 回调在 Rust 内部的 tokio 工作线程触发，实现方需自行保证线程安全。</p>
 */
public interface DhtListener {

    /**
     * 成功获取到 torrent metadata 时触发。
     *
     * @param info 完整的 torrent 信息对象，由 Rust JNI 层映射构造
     */
    void onTorrent(TorrentInfo info);

    /**
     * 内部发生错误时触发。
     *
     * @param message Rust 侧错误的文本描述
     */
    void onError(String message);

    /**
     * 在即将对某 info_hash 拉取 BitTorrent metadata 之前调用。
     * 与 Rust {@code DHTServer::on_metadata_fetch} 对应：返回 {@code true} 才会真正去拉取；
     * 返回 {@code false} 则跳过该 hash，不会触发 {@link #onTorrent}。
     * <p>在 Rust 的 metadata 工作线程中通过阻塞线程池调用，应快速返回，避免长时间阻塞。</p>
     *
     * @param infoHash 40 字符小写十六进制 info_hash
     * @return 是否允许拉取 metadata
     */
    default boolean onMetadataFetch(String infoHash) {
        return true;
    }
}
