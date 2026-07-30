use crate::{DHTOptions, FileInfo, MetadataOptions, TorrentInfo, types::NetMode};
use jni::JNIEnv;
use jni::objects::{JObject, JString, JValue};
use jni::sys::jlong;

#[derive(Debug, thiserror::Error)]
pub enum DhtOptionsConversionError {
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
    #[error("invalid DHTOptions.{field}: expected {expected}, got {value}")]
    InvalidValue {
        field: &'static str,
        expected: &'static str,
        value: i64,
    },
}

type DhtOptionsResult<T> = Result<T, DhtOptionsConversionError>;

fn invalid_value(
    field: &'static str,
    expected: &'static str,
    value: impl Into<i64>,
) -> DhtOptionsConversionError {
    DhtOptionsConversionError::InvalidValue {
        field,
        expected,
        value: value.into(),
    }
}

fn checked_port(value: i32) -> DhtOptionsResult<u16> {
    u16::try_from(value).map_err(|_| invalid_value("port", "an integer in 0..=65535", value))
}

fn checked_non_negative_u32(field: &'static str, value: i32) -> DhtOptionsResult<u32> {
    u32::try_from(value).map_err(|_| invalid_value(field, "a non-negative integer", value))
}

fn checked_non_negative_u64(field: &'static str, value: i64) -> DhtOptionsResult<u64> {
    u64::try_from(value).map_err(|_| invalid_value(field, "a non-negative integer", value))
}

fn checked_non_negative_usize(field: &'static str, value: i32) -> DhtOptionsResult<usize> {
    usize::try_from(value).map_err(|_| invalid_value(field, "a non-negative integer", value))
}

fn checked_positive_usize(field: &'static str, value: i32) -> DhtOptionsResult<usize> {
    let value = checked_non_negative_usize(field, value)?;
    if value == 0 {
        return Err(invalid_value(
            field,
            "an integer greater than or equal to 1",
            0,
        ));
    }
    Ok(value)
}

fn checked_percentage(field: &'static str, value: i32) -> DhtOptionsResult<u8> {
    if !(0..=100).contains(&value) {
        return Err(invalid_value(field, "an integer in 0..=100", value));
    }
    Ok(u8::try_from(value).expect("0..=100 always fits in u8"))
}

fn checked_netmode(value: i32) -> DhtOptionsResult<NetMode> {
    match value {
        0 => Ok(NetMode::Ipv4Only),
        1 => Ok(NetMode::Ipv6Only),
        2 => Ok(NetMode::DualStack),
        _ => Err(invalid_value("netMode", "one of 0, 1, or 2", value)),
    }
}

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
    env.with_local_frame_returning_local(4, |env| {
        let cls = env.find_class(CLASS_FILE_INFO)?;
        let path = rust_str_to_jstring(env, &fi.path)?;
        env.new_object(
            &cls,
            "(Ljava/lang/String;J)V",
            &[JValue::Object(&path), JValue::Long(fi.size as jlong)],
        )
    })
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

/// 从 JVM bindings 的 options DTO 读取字段，构造 Rust `DHTOptions`。
pub fn java_to_dht_options(env: &mut JNIEnv, obj: &JObject) -> DhtOptionsResult<DHTOptions> {
    let port = checked_port(env.get_field(obj, "port", "I")?.i()?)?;
    let metadata_timeout_secs = checked_non_negative_u64(
        "metadataTimeout",
        env.get_field(obj, "metadataTimeout", "J")?.j()?,
    )?;
    let metadata_max_queue_size = checked_positive_usize(
        "maxMetadataQueueSize",
        env.get_field(obj, "maxMetadataQueueSize", "I")?.i()?,
    )?;
    let metadata_max_worker_count = checked_positive_usize(
        "maxMetadataWorkerCount",
        env.get_field(obj, "maxMetadataWorkerCount", "I")?.i()?,
    )?;
    let pool_capacity = checked_positive_usize(
        "poolCapacity",
        env.get_field(obj, "poolCapacity", "I")?.i()?,
    )?;
    let find_node_rate = checked_non_negative_u32(
        "findNodeRatePerSecond",
        env.get_field(obj, "findNodeRatePerSecond", "I")?.i()?,
    )?;
    let find_node_burst = checked_non_negative_u32(
        "findNodeBurst",
        env.get_field(obj, "findNodeBurst", "I")?.i()?,
    )?;
    let max_find_node_in_flight = checked_positive_usize(
        "maxFindNodeInFlight",
        env.get_field(obj, "maxFindNodeInFlight", "I")?.i()?,
    )?;
    let max_new_destinations = checked_non_negative_u32(
        "maxNewDestinationsPerMinute",
        env.get_field(obj, "maxNewDestinationsPerMinute", "I")?
            .i()?,
    )?;
    let max_replacements = checked_non_negative_u32(
        "maxReplacementsPerMinute",
        env.get_field(obj, "maxReplacementsPerMinute", "I")?.i()?,
    )?;
    let request_timeout_secs = checked_non_negative_u64(
        "requestTimeoutSeconds",
        env.get_field(obj, "requestTimeoutSeconds", "J")?.j()?,
    )?;
    let max_response_rate = checked_non_negative_u32(
        "maxResponseRatePerSecond",
        env.get_field(obj, "maxResponseRatePerSecond", "I")?.i()?,
    )?;
    let max_response_bytes = checked_non_negative_u64(
        "maxResponseBytesPerSecond",
        env.get_field(obj, "maxResponseBytesPerSecond", "J")?.j()?,
    )?;
    let max_response_per_source = checked_non_negative_u32(
        "maxResponseRatePerSource",
        env.get_field(obj, "maxResponseRatePerSource", "I")?.i()?,
    )?;
    let pressure_floor = checked_percentage(
        "metadataPressureFloorPercent",
        env.get_field(obj, "metadataPressureFloorPercent", "I")?
            .i()?,
    )?;
    let recent_probe_ttl = checked_non_negative_u64(
        "recentProbeTtlSeconds",
        env.get_field(obj, "recentProbeTtlSeconds", "J")?.j()?,
    )?;
    let responsive_capacity = checked_positive_usize(
        "responsiveCapacity",
        env.get_field(obj, "responsiveCapacity", "I")?.i()?,
    )?;
    let responsive_ttl = checked_non_negative_u64(
        "responsiveTtlSeconds",
        env.get_field(obj, "responsiveTtlSeconds", "J")?.j()?,
    )?;
    let low_watermark = checked_non_negative_usize(
        "poolLowWatermark",
        env.get_field(obj, "poolLowWatermark", "I")?.i()?,
    )?;
    if low_watermark > pool_capacity {
        return Err(invalid_value(
            "poolLowWatermark",
            "an integer no greater than poolCapacity",
            i64::try_from(low_watermark).expect("Java int always fits in i64"),
        ));
    }
    let subnet_in_flight = checked_positive_usize(
        "maxInFlightPerSubnet",
        env.get_field(obj, "maxInFlightPerSubnet", "I")?.i()?,
    )?;
    let hash_queue_capacity = checked_positive_usize(
        "hashQueueCapacity",
        env.get_field(obj, "hashQueueCapacity", "I")?.i()?,
    )?;
    let netmode = checked_netmode(env.get_field(obj, "netMode", "I")?.i()?)?;

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
    options.crawl.pool.capacity = pool_capacity;
    options.crawl.pool.recent_probe_ttl_secs = recent_probe_ttl;
    options.crawl.pool.responsive_capacity = responsive_capacity;
    options.crawl.pool.responsive_ttl_secs = responsive_ttl;
    options.crawl.pool.low_watermark = low_watermark;
    options.crawl.rate_limit.max_find_node_rate_per_sec = find_node_rate;
    options.crawl.rate_limit.burst = find_node_burst;
    options.crawl.rate_limit.max_in_flight = max_find_node_in_flight;
    options.crawl.rate_limit.max_new_destinations_per_minute = max_new_destinations;
    options.crawl.rate_limit.request_timeout_secs = request_timeout_secs;
    options.crawl.rate_limit.max_response_rate_per_sec = max_response_rate;
    options.crawl.rate_limit.max_response_bytes_per_sec = max_response_bytes;
    options.crawl.rate_limit.max_response_rate_per_source = max_response_per_source;
    options.crawl.rate_limit.metadata_pressure_floor_percent = pressure_floor;
    options.crawl.rate_limit.max_replacements_per_minute = max_replacements;
    options.crawl.rate_limit.max_in_flight_per_subnet = subnet_in_flight;
    Ok(options)
}

/// 从 JVM bindings 的 options DTO 读取，或若为 null 则返回默认选项。
pub fn java_to_dht_options_or_default(
    env: &mut JNIEnv,
    obj: &JObject,
) -> DhtOptionsResult<DHTOptions> {
    if obj.is_null() {
        Ok(DHTOptions::default())
    } else {
        java_to_dht_options(env, obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invalid<T>(result: DhtOptionsResult<T>, field: &'static str, value: i64) {
        match result {
            Err(DhtOptionsConversionError::InvalidValue {
                field: actual_field,
                value: actual_value,
                ..
            }) => {
                assert_eq!(actual_field, field);
                assert_eq!(actual_value, value);
            }
            _ => panic!("expected an invalid-value error"),
        }
    }

    #[test]
    fn port_accepts_java_boundaries_and_rejects_out_of_range_values() {
        assert_eq!(checked_port(0).unwrap(), 0);
        assert_eq!(checked_port(i32::from(u16::MAX)).unwrap(), u16::MAX);
        assert_invalid(checked_port(-1), "port", -1);
        assert_invalid(checked_port(i32::from(u16::MAX) + 1), "port", 65_536);
    }

    #[test]
    fn signed_values_are_checked_before_unsigned_conversion() {
        assert_eq!(checked_non_negative_u32("rate", 0).unwrap(), 0);
        assert_eq!(
            checked_non_negative_u32("rate", i32::MAX).unwrap(),
            i32::MAX as u32
        );
        assert_invalid(checked_non_negative_u32("rate", -1), "rate", -1);

        assert_eq!(checked_non_negative_u64("timeout", 0).unwrap(), 0);
        assert_eq!(
            checked_non_negative_u64("timeout", i64::MAX).unwrap(),
            i64::MAX as u64
        );
        assert_invalid(checked_non_negative_u64("timeout", -1), "timeout", -1);
    }

    #[test]
    fn capacities_and_in_flight_limits_must_be_positive() {
        assert_eq!(checked_positive_usize("capacity", 1).unwrap(), 1);
        assert_eq!(
            checked_positive_usize("capacity", i32::MAX).unwrap(),
            i32::MAX as usize
        );
        assert_invalid(checked_positive_usize("capacity", 0), "capacity", 0);
        assert_invalid(checked_positive_usize("capacity", -1), "capacity", -1);
    }

    #[test]
    fn percentage_and_netmode_only_accept_documented_values() {
        assert_eq!(checked_percentage("percent", 0).unwrap(), 0);
        assert_eq!(checked_percentage("percent", 100).unwrap(), 100);
        assert_invalid(checked_percentage("percent", -1), "percent", -1);
        assert_invalid(checked_percentage("percent", 101), "percent", 101);

        assert_eq!(checked_netmode(0).unwrap(), NetMode::Ipv4Only);
        assert_eq!(checked_netmode(1).unwrap(), NetMode::Ipv6Only);
        assert_eq!(checked_netmode(2).unwrap(), NetMode::DualStack);
        assert_invalid(checked_netmode(-1), "netMode", -1);
        assert_invalid(checked_netmode(3), "netMode", 3);
    }
}
