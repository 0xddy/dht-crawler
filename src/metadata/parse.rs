use super::FetchedMetadata;
use super::bencode::{self, BencodeValue};
use crate::types::FileInfo;

fn decode_text(value: &BencodeValue) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_owned());
    }
    value
        .as_bytes()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

pub(crate) fn parse_metadata(info_bytes: &[u8]) -> Option<FetchedMetadata> {
    let value = bencode::decode_exact(info_bytes).ok()?;
    let dict = value.as_dict()?;
    let name = dict
        .get(&b"name.utf-8"[..])
        .and_then(decode_text)
        .or_else(|| dict.get(&b"name"[..]).and_then(decode_text))
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
                .or_else(|| file_dict.get(&b"path"[..]))
                .and_then(BencodeValue::as_list)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(decode_text)
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
