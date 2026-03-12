use crate::{TorrentInfo, FileInfo, DHTOptions, types::NetMode};
use jni::JNIEnv;
use jni::objects::{JObject, JString, JValue};
use jni::sys::jlong;

// ──────────────────────────────────────────────────────────────────────────────
// 常量：Java 类全限定名
// ──────────────────────────────────────────────────────────────────────────────
const CLASS_TORRENT_INFO: &str = "cn/lmcw/dht/model/TorrentInfo";
const CLASS_FILE_INFO: &str = "cn/lmcw/dht/model/FileInfo";
const CLASS_DHT_OPTIONS: &str = "cn/lmcw/dht/model/DHTOptions";
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

/// Java String（JObject）→ Rust String
pub fn jstring_to_rust(env: &mut JNIEnv, obj: &JObject) -> jni::errors::Result<String> {
    let jstr = JString::from(env.new_local_ref(obj)?);
    Ok(env.get_string(&jstr)?.into())
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
        env.call_method(&list, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(&jfi)])?;
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
        env.call_method(&list, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(&js)])?;
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
    let metadata_timeout = env.get_field(obj, "metadataTimeout", "J")?.j()? as u64;
    let max_metadata_queue_size =
        env.get_field(obj, "maxMetadataQueueSize", "I")?.i()? as usize;
    let max_metadata_worker_count =
        env.get_field(obj, "maxMetadataWorkerCount", "I")?.i()? as usize;
    let node_queue_capacity =
        env.get_field(obj, "nodeQueueCapacity", "I")?.i()? as usize;
    let hash_queue_capacity =
        env.get_field(obj, "hashQueueCapacity", "I")?.i()? as usize;
    let netmode_ord = env.get_field(obj, "netMode", "I")?.i()?;
    let netmode = match netmode_ord {
        0 => NetMode::Ipv4Only,
        1 => NetMode::Ipv6Only,
        _ => NetMode::DualStack,
    };

    Ok(DHTOptions {
        port,
        metadata_timeout,
        max_metadata_queue_size,
        max_metadata_worker_count,
        netmode,
        node_queue_capacity,
        hash_queue_capacity,
    })
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
