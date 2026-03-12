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
}
