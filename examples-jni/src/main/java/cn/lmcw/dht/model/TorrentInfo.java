package cn.lmcw.dht.model;

import java.util.List;

/**
 * 与 Rust {@code TorrentInfo} 一一对应的 POJO。
 * <p>由 Rust JNI 层通过 {@code NewObject} / {@code SetField} 构造并填充，
 * 通过 {@link cn.lmcw.dht.DhtListener#onTorrent(TorrentInfo)} 回调给 Java 层。</p>
 */
public final class TorrentInfo {

    private final String infoHash;
    private final String magnetLink;
    private final String name;
    private final long totalSize;
    private final List<FileInfo> files;
    private final long pieceLength;
    private final List<String> peers;
    private final long timestamp;

    /**
     * 供 JNI 层调用的全参构造器。
     *
     * @param infoHash    十六进制 info-hash 字符串
     * @param magnetLink  magnet 链接
     * @param name        种子名称
     * @param totalSize   总字节数
     * @param files       文件列表
     * @param pieceLength 分片长度（字节）
     * @param peers       来源节点地址列表
     * @param timestamp   发现时间戳（Unix 秒）
     */
    public TorrentInfo(
            String infoHash,
            String magnetLink,
            String name,
            long totalSize,
            List<FileInfo> files,
            long pieceLength,
            List<String> peers,
            long timestamp) {
        this.infoHash = infoHash;
        this.magnetLink = magnetLink;
        this.name = name;
        this.totalSize = totalSize;
        this.files = files;
        this.pieceLength = pieceLength;
        this.peers = peers;
        this.timestamp = timestamp;
    }

    public String getInfoHash() { return infoHash; }
    public String getMagnetLink() { return magnetLink; }
    public String getName() { return name; }
    public long getTotalSize() { return totalSize; }
    public List<FileInfo> getFiles() { return files; }
    public long getPieceLength() { return pieceLength; }
    public List<String> getPeers() { return peers; }
    public long getTimestamp() { return timestamp; }

    @Override
    public String toString() {
        return "TorrentInfo{"
                + "infoHash='" + infoHash + '\''
                + ", name='" + name + '\''
                + ", totalSize=" + totalSize
                + ", files=" + (files != null ? files.size() : 0)
                + '}';
    }
}
