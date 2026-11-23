use super::FetchedMetadata;
use super::bencode::{self, BencodeValue};
use crate::types::FileInfo;
use chardetng::EncodingDetector;
use encoding_rs::{BIG5, EUC_KR, Encoding, GB18030, SHIFT_JIS, WINDOWS_1251, WINDOWS_1252};

fn codepage_to_encoding(codepage: i64) -> Option<&'static Encoding> {
    match codepage {
        936 | 54936 => Some(GB18030),
        950 => Some(BIG5),
        932 => Some(SHIFT_JIS),
        949 => Some(EUC_KR),
        1251 => Some(WINDOWS_1251),
        1252 => Some(WINDOWS_1252),
        _ => None,
    }
}

/// 严格检查非 ASCII 字节是否全部由 GB2312/GBK 双高字节（0xA1..=0xFE）成对组成，
/// 避免将西欧单字节重音（如 Latin-1 `Amélie` 中的 `0xE9 0x6C`）误判为 GB18030。
fn looks_like_strict_gb2312_pairs(chunks: &[&[u8]]) -> bool {
    let mut pair_count = 0usize;
    for bytes in chunks {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b < 0x80 {
                i += 1;
                continue;
            }
            if (0xA1..=0xFE).contains(&b)
                && i + 1 < bytes.len()
                && (0xA1..=0xFE).contains(&bytes[i + 1])
            {
                pair_count += 1;
                i += 2;
            } else {
                return false;
            }
        }
    }
    pair_count >= 2
}

/// 检查字节流是否包含 CP1252 未定义控制字节或孤立的 `0xFF`（常见截断/二进制损坏字节），
/// 避免把明显损坏的非文本字节误当作西欧 Windows-1252 强行解码成 `ÿ`。
fn has_suspicious_win1252_bytes(chunks: &[&[u8]]) -> bool {
    chunks.iter().any(|bytes| {
        bytes
            .iter()
            .any(|&b| matches!(b, 0x81 | 0x8D | 0x8F | 0x90 | 0x9D | 0xFF))
    })
}

/// 收集种子内全部非 UTF-8 文本原始字节并联合检测整包传统编码（GB18030/Big5/Shift_JIS/EUC-KR/Win1251/Win1252）。
fn detect_legacy_encoding(
    explicit_codepage: Option<i64>,
    non_utf8_chunks: &[&[u8]],
) -> Option<&'static Encoding> {
    if non_utf8_chunks.is_empty() {
        return None;
    }
    if let Some(enc) = explicit_codepage.and_then(codepage_to_encoding) {
        return Some(enc);
    }

    let mut detector = EncodingDetector::new();
    for (idx, chunk) in non_utf8_chunks.iter().enumerate() {
        if idx > 0 {
            detector.feed(b"\n", false);
        }
        detector.feed(chunk, idx + 1 == non_utf8_chunks.len());
    }
    let (guessed, confident) = detector.guess_assess(None, false);

    // 若 chardetng 明确识别出东亚/俄文/西欧编码，直接采用（排除含损坏字节的默认 Win1252 兜底）
    if confident {
        let win1252_suspicious =
            guessed == WINDOWS_1252 && has_suspicious_win1252_bytes(non_utf8_chunks);
        if !win1252_suspicious {
            let any_clean = non_utf8_chunks
                .iter()
                .any(|c| !guessed.decode_without_bom_handling(c).1);
            if any_clean {
                return Some(guessed);
            }
        }
    }

    // 短串兜底：老中文种子常仅有 2~6 个 GB2312 汉字，chardetng 置信度不足时检查严格双高字节 GB18030
    if looks_like_strict_gb2312_pairs(non_utf8_chunks) {
        let all_clean = non_utf8_chunks.iter().all(|c| {
            let (decoded, had_errors) = GB18030.decode_without_bom_handling(c);
            !had_errors
                && decoded
                    .chars()
                    .any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch))
        });
        if all_clean {
            return Some(GB18030);
        }
    }

    // 若 chardetng 给出非 WINDOWS_1252 的特定编码且能无错解码，也采用（例如较短的 Shift_JIS/Big5/EUC-KR/Win1251）
    if guessed != WINDOWS_1252 {
        let all_clean = non_utf8_chunks
            .iter()
            .all(|c| !guessed.decode_without_bom_handling(c).1);
        if all_clean {
            return Some(guessed);
        }
    }

    None
}

fn decode_text(
    value: &BencodeValue,
    fallback_encoding: Option<&'static Encoding>,
) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_owned());
    }
    let bytes = value.as_bytes()?;
    if let Some(enc) = fallback_encoding {
        let (decoded, _) = enc.decode_without_bom_handling(bytes);
        return Some(decoded.into_owned());
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn pick_preferred_raw<'a>(
    utf8_val: Option<&'a BencodeValue>,
    raw_val: Option<&'a BencodeValue>,
) -> Option<&'a BencodeValue> {
    if let Some(v) = utf8_val
        && v.as_str().is_some()
    {
        return Some(v);
    }
    raw_val.or(utf8_val)
}

pub(crate) fn parse_metadata(info_bytes: &[u8]) -> Option<FetchedMetadata> {
    let value = bencode::decode_exact(info_bytes).ok()?;
    let dict = value.as_dict()?;

    let explicit_codepage = dict
        .get(&b"codepage"[..])
        .and_then(BencodeValue::as_integer);

    let chosen_name_val = pick_preferred_raw(dict.get(&b"name.utf-8"[..]), dict.get(&b"name"[..]));

    // 预扫描所有待解码的原始文本字段，收集非 UTF-8 字节以做整包联合编码检测
    let mut non_utf8_chunks: Vec<&[u8]> = Vec::new();
    if let Some(v) = chosen_name_val
        && v.as_str().is_none()
        && let Some(bytes) = v.as_bytes()
    {
        non_utf8_chunks.push(bytes);
    }

    if let Some(files) = dict.get(&b"files"[..]).and_then(BencodeValue::as_list) {
        for file in files {
            let Some(file_dict) = file.as_dict() else {
                continue;
            };
            let path_list = file_dict
                .get(&b"path.utf-8"[..])
                .and_then(BencodeValue::as_list)
                .filter(|parts| parts.iter().all(|p| p.as_str().is_some()))
                .or_else(|| file_dict.get(&b"path"[..]).and_then(BencodeValue::as_list));
            if let Some(parts) = path_list {
                for part in parts {
                    if part.as_str().is_none()
                        && let Some(bytes) = part.as_bytes()
                    {
                        non_utf8_chunks.push(bytes);
                    }
                }
            }
        }
    }

    let fallback_encoding = detect_legacy_encoding(explicit_codepage, &non_utf8_chunks);

    let name = chosen_name_val
        .and_then(|v| decode_text(v, fallback_encoding))
        .unwrap_or_else(|| "Unknown".to_string());
    let piece_length = dict
        .get(&b"piece length"[..])
        .and_then(BencodeValue::as_integer)
        .and_then(|len| u64::try_from(len).ok())
        .unwrap_or(0);

    let mut total_size = 0u64;
    let mut file_list = Vec::new();
    if let Some(files) = dict.get(&b"files"[..]).and_then(BencodeValue::as_list) {
        for file in files {
            let Some(file_dict) = file.as_dict() else {
                continue;
            };
            let Some(length) = file_dict
                .get(&b"length"[..])
                .and_then(BencodeValue::as_integer)
                .and_then(|len| u64::try_from(len).ok())
            else {
                continue;
            };
            total_size = total_size.saturating_add(length);
            let path = file_dict
                .get(&b"path.utf-8"[..])
                .and_then(BencodeValue::as_list)
                .filter(|parts| parts.iter().all(|p| p.as_str().is_some()))
                .or_else(|| file_dict.get(&b"path"[..]).and_then(BencodeValue::as_list))
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| decode_text(p, fallback_encoding))
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            file_list.push(FileInfo { path, size: length });
        }
    } else if let Some(length) = dict
        .get(&b"length"[..])
        .and_then(BencodeValue::as_integer)
        .and_then(|len| u64::try_from(len).ok())
    {
        total_size = length;
        file_list.push(FileInfo {
            path: name.clone(),
            size: total_size,
        });
    }

    (total_size > 0).then_some((name, total_size, file_list, piece_length))
}
