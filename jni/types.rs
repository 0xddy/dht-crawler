use crate::{DHTOptions, FileInfo, MetadataOptions, TorrentInfo, types::NetMode};
use jni::JNIEnv;
use jni::objects::{JObject, JString, JValue};
use jni::sys::jlong;

// ──────────────────────────────────────────────────────────────────────────────
// 常量：Java 类全限定名
// ──────────────────────────────────────────────────────────────────────────────
const CLASS_TORRENT_INFO: &str = "cn/lmcw/dht/model/TorrentInfo";
const CLASS_FILE_INFO: &str = "cn/lmcw/dht/model/FileInfo";
const CLASS_ARRAY_LIST: &str = "java/util/ArrayList";

// ──────────────────────────────────────────────────────────────────────────────
// 基础类型转换
// ──────────────────────────────────────────────────────────────────────────────

/// Rust String → Java String（jstring）
pub fn rust_str_to_jstring<'local>(
    env: &mut JNIEnv<'local>,
    s: &str,
) -> jni::errors::Result<JObject<'local>> {
    let js: JString<'local> = env.new_string(s)?;
    Ok(js.into())
}

// ──────────────────────────────────────────────────────────────────────────────
// FileInfo: Rust → Java
// ──────────────────────────────────────────────────────────────────────────────

/// 将 Rust `FileInfo` 构造为 Java `cn.lmcw.dht.model.FileInfo` 对象。
pub fn file_info_to_java<'local>(
    env: &mut JNIEnv<'local>,
    fi: &FileInfo,
) -> jni::errors::Result<JObject<'local>> {
    let cls = env.find_class(CLASS_FILE_INFO)?;
    let path = rust_str_to_jstring(env, &fi.path)?;
    let obj = env.new_object(
        &cls,
        "(Ljava/lang/String;J)V",
        &[JValue::Object(&path), JValue::Long(fi.size as jlong)],
    )?;
    Ok(obj)
}

// ──────────────────────────────────────────────────────────────────────────────
// TorrentInfo: Rust → Java
// ──────────────────────────────────────────────────────────────────────────────

/// 将 Rust `TorrentInfo` 构造为 Java `cn.lmcw.dht.model.TorrentInfo` 对象。
/// files 字段被构造为 `java.util.ArrayList<FileInfo>`。
pub fn torrent_info_to_java<'local>(
    env: &mut JNIEnv<'local>,
    ti: &TorrentInfo,
) -> jni::errors::Result<JObject<'local>> {
    let cls = env.find_class(CLASS_TORRENT_INFO)?;

    // 构造 files list
    let list = build_file_list(env, &ti.files)?;

    // 构造 peers list（List<String>）
    let peers_list = build_string_list(env, &ti.peers)?;

    let info_hash = rust_str_to_jstring(env, &ti.info_hash)?;
    let magnet = rust_str_to_jstring(env, &ti.magnet_link)?;
    let name = rust_str_to_jstring(env, &ti.name)?;

    let obj = env.new_object(
        &cls,
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JLjava/util/List;JLjava/util/List;J)V",
        &[
            JValue::Object(&info_hash),
            JValue::Object(&magnet),
            JValue::Object(&name),
            JValue::Long(ti.total_size as jlong),
            JValue::Object(&list),
            JValue::Long(ti.piece_length as jlong),
            JValue::Object(&peers_list),
            JValue::Long(ti.timestamp as jlong),
        ],
    )?;
    Ok(obj)
}

/// 构造 `java.util.ArrayList` 并填入 FileInfo 对象列表。
fn build_file_list<'local>(
    env: &mut JNIEnv<'local>,
    files: &[FileInfo],
) -> jni::errors::Result<JObject<'local>> {
    let list_cls = env.find_class(CLASS_ARRAY_LIST)?;
    let list = env.new_object(&list_cls, "()V", &[])?;
    for fi in files {
        let jfi = file_info_to_java(env, fi)?;
        env.call_method(
            &list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[JValue::Object(&jfi)],
        )?;
        env.delete_local_ref(jfi)?;
    }
    Ok(list)
}

/// 构造 `java.util.ArrayList` 并填入字符串列表。
fn build_string_list<'local>(
    env: &mut JNIEnv<'local>,
    strs: &[String],
) -> jni::errors::Result<JObject<'local>> {
    let list_cls = env.find_class(CLASS_ARRAY_LIST)?;
    let list = env.new_object(&list_cls, "()V", &[])?;
    for s in strs {
        let js = rust_str_to_jstring(env, s)?;
        env.call_method(
            &list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[JValue::Object(&js)],
        )?;
        env.delete_local_ref(js)?;
    }
    Ok(list)
}

// ──────────────────────────────────────────────────────────────────────────────
// DHTOptions: Java → Rust
// ──────────────────────────────────────────────────────────────────────────────

/// 从 Java `cn.lmcw.dht.model.DHTOptions` 对象读取字段，构造 Rust `DHTOptions`。
pub fn java_to_dht_options(env: &mut JNIEnv, obj: &JObject) -> jni::errors::Result<DHTOptions> {
    let port = env.get_field(obj, "port", "I")?.i()? as u16;
    let metadata_timeout_secs = env.get_field(obj, "metadataTimeout", "J")?.j()? as u64;
    let metadata_max_queue_size = env.get_field(obj, "maxMetadataQueueSize", "I")?.i()? as usize;
    let metadata_max_worker_count =
        env.get_field(obj, "maxMetadataWorkerCount", "I")?.i()? as usize;
    let pool_capacity = env.get_field(obj, "poolCapacity", "I")?.i()? as usize;
    let find_node_rate = env.get_field(obj, "findNodeRatePerSecond", "I")?.i()? as u32;
    let find_node_burst = env.get_field(obj, "findNodeBurst", "I")?.i()? as u32;
    let max_find_node_in_flight = env.get_field(obj, "maxFindNodeInFlight", "I")?.i()? as usize;
    let max_new_destinations = env
        .get_field(obj, "maxNewDestinationsPerMinute", "I")?
        .i()? as u32;
    let max_replacements = env.get_field(obj, "maxReplacementsPerMinute", "I")?.i()? as u32;
    let request_timeout_secs = env.get_field(obj, "requestTimeoutSeconds", "J")?.j()? as u64;
    let max_response_rate = env.get_field(obj, "maxResponseRatePerSecond", "I")?.i()? as u32;
    let max_response_bytes = env.get_field(obj, "maxResponseBytesPerSecond", "J")?.j()? as u64;
    let max_response_per_source = env.get_field(obj, "maxResponseRatePerSource", "I")?.i()? as u32;
    let pressure_floor = env
        .get_field(obj, "metadataPressureFloorPercent", "I")?
        .i()? as u8;
    let recent_probe_ttl = env.get_field(obj, "recentProbeTtlSeconds", "J")?.j()? as u64;
    let responsive_capacity = env.get_field(obj, "responsiveCapacity", "I")?.i()? as usize;
    let responsive_ttl = env.get_field(obj, "responsiveTtlSeconds", "J")?.j()? as u64;
    let low_watermark = env.get_field(obj, "poolLowWatermark", "I")?.i()? as usize;
    let subnet_in_flight = env.get_field(obj, "maxInFlightPerSubnet", "I")?.i()? as usize;
    let hash_queue_capacity = env.get_field(obj, "hashQueueCapacity", "I")?.i()? as usize;
    let netmode_ord = env.get_field(obj, "netMode", "I")?.i()?;
    let netmode = match netmode_ord {
        0 => NetMode::Ipv4Only,
        1 => NetMode::Ipv6Only,
        _ => NetMode::DualStack,
    };

    let mut options = DHTOptions {
        port,
        netmode,
        hash_queue_capacity,
        metadata: MetadataOptions {
            timeout_secs: metadata_timeout_secs,
            max_queue_size: metadata_max_queue_size,
            max_worker_count: metadata_max_worker_count,
            ..MetadataOptions::default()
        },
        ..DHTOptions::default()
    };
    options.crawl.pool.capacity = pool_capacity.max(1);
    options.crawl.pool.recent_probe_ttl_secs = recent_probe_ttl;
    options.crawl.pool.responsive_capacity = responsive_capacity.max(1);
    options.crawl.pool.responsive_ttl_secs = responsive_ttl;
    options.crawl.pool.low_watermark = low_watermark.min(pool_capacity);
    options.crawl.rate_limit.max_find_node_rate_per_sec = find_node_rate;
    options.crawl.rate_limit.burst = find_node_burst;
    options.crawl.rate_limit.max_in_flight = max_find_node_in_flight.max(1);
    options.crawl.rate_limit.max_new_destinations_per_minute = max_new_destinations;
    options.crawl.rate_limit.request_timeout_secs = request_timeout_secs;
    options.crawl.rate_limit.max_response_rate_per_sec = max_response_rate;
    options.crawl.rate_limit.max_response_bytes_per_sec = max_response_bytes;
    options.crawl.rate_limit.max_response_rate_per_source = max_response_per_source;
    options.crawl.rate_limit.metadata_pressure_floor_percent = pressure_floor.min(100);
    options.crawl.rate_limit.max_replacements_per_minute = max_replacements;
    options.crawl.rate_limit.max_in_flight_per_subnet = subnet_in_flight.max(1);
    Ok(options)
}

/// 从 Java `cn.lmcw.dht.model.DHTOptions` 对象读取，或若为 null 则返回默认选项。
pub fn java_to_dht_options_or_default(
    env: &mut JNIEnv,
    obj: &JObject,
) -> jni::errors::Result<DHTOptions> {
    if obj.is_null() {
        Ok(DHTOptions::default())
    } else {
        java_to_dht_options(env, obj)
    }
}
