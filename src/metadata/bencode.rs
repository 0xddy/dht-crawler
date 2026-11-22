use bytes::Bytes;
use std::collections::BTreeMap;
use std::fmt;

const MAX_BENCODE_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BencodeValue {
    Integer(i64),
    Bytes(Bytes),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Bytes, BencodeValue>),
}

impl BencodeValue {
    #[cfg(test)]
    pub(crate) fn string(value: &str) -> Self {
        Self::Bytes(Bytes::copy_from_slice(value.as_bytes()))
    }

    pub(crate) fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(v) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b.as_ref()),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        self.as_bytes()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    pub(crate) fn as_list(&self) -> Option<&[BencodeValue]> {
        match self {
            Self::List(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub(crate) fn as_dict(&self) -> Option<&BTreeMap<Bytes, BencodeValue>> {
        match self {
            Self::Dict(map) => Some(map),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BencodeError {
    UnexpectedEof,
    InvalidSyntax,
    IntegerOverflow,
    DepthLimit,
    TrailingBytes,
}

impl fmt::Display for BencodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("unexpected end of bencode input"),
            Self::InvalidSyntax => f.write_str("invalid bencode syntax"),
            Self::IntegerOverflow => f.write_str("bencode integer overflow"),
            Self::DepthLimit => f.write_str("bencode nesting depth limit exceeded"),
            Self::TrailingBytes => f.write_str("unexpected trailing bytes after bencode value"),
        }
    }
}

impl std::error::Error for BencodeError {}

pub(crate) fn decode_prefix(input: &[u8]) -> Result<(BencodeValue, usize), BencodeError> {
    let mut pos = 0usize;
    let value = decode_at(input, &mut pos, 0)?;
    Ok((value, pos))
}

pub(crate) fn decode_exact(input: &[u8]) -> Result<BencodeValue, BencodeError> {
    let (value, consumed) = decode_prefix(input)?;
    if consumed != input.len() {
        return Err(BencodeError::TrailingBytes);
    }
    Ok(value)
}

pub(crate) fn encode(value: &BencodeValue) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &BencodeValue, out: &mut Vec<u8>) {
    match value {
        BencodeValue::Integer(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        BencodeValue::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        BencodeValue::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        BencodeValue::Dict(map) => {
            out.push(b'd');
            for (k, v) in map {
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

fn decode_at(input: &[u8], pos: &mut usize, depth: usize) -> Result<BencodeValue, BencodeError> {
    let tag = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
    match tag {
        b'i' => {
            *pos += 1;
            decode_integer(input, pos).map(BencodeValue::Integer)
        }
        b'l' => {
            if depth >= MAX_BENCODE_DEPTH {
                return Err(BencodeError::DepthLimit);
            }
            *pos += 1;
            let mut items = Vec::new();
            loop {
                let next = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
                if next == b'e' {
                    *pos += 1;
                    return Ok(BencodeValue::List(items));
                }
                items.push(decode_at(input, pos, depth + 1)?);
            }
        }
        b'd' => {
            if depth >= MAX_BENCODE_DEPTH {
                return Err(BencodeError::DepthLimit);
            }
            *pos += 1;
            let mut map = BTreeMap::new();
            loop {
                let next = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
                if next == b'e' {
                    *pos += 1;
                    return Ok(BencodeValue::Dict(map));
                }
                if !next.is_ascii_digit() {
                    return Err(BencodeError::InvalidSyntax);
                }
                let key = decode_bytes(input, pos)?;
                let val = decode_at(input, pos, depth + 1)?;
                map.insert(key, val);
            }
        }
        b'0'..=b'9' => decode_bytes(input, pos).map(BencodeValue::Bytes),
        _ => Err(BencodeError::InvalidSyntax),
    }
}

fn decode_integer(input: &[u8], pos: &mut usize) -> Result<i64, BencodeError> {
    let start = *pos;
    let negative = match input.get(*pos) {
        Some(b'-') => {
            *pos += 1;
            true
        }
        Some(_) => false,
        None => return Err(BencodeError::UnexpectedEof),
    };

    let digit_start = *pos;
    let first = *input.get(digit_start).ok_or(BencodeError::UnexpectedEof)?;
    if !first.is_ascii_digit() {
        return Err(BencodeError::InvalidSyntax);
    }
    if first == b'0' && negative {
        return Err(BencodeError::InvalidSyntax);
    }

    let mut acc = 0i64;
    let mut digits = 0usize;
    loop {
        let byte = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
        if byte == b'e' {
            if digits == 0 || *pos == start {
                return Err(BencodeError::InvalidSyntax);
            }
            *pos += 1;
            return Ok(acc);
        }
        if !byte.is_ascii_digit() {
            return Err(BencodeError::InvalidSyntax);
        }
        if digits == 1 && first == b'0' {
            return Err(BencodeError::InvalidSyntax);
        }
        let digit = i64::from(byte - b'0');
        acc = if negative {
            acc.checked_mul(10)
                .and_then(|v| v.checked_sub(digit))
                .ok_or(BencodeError::IntegerOverflow)?
        } else {
            acc.checked_mul(10)
                .and_then(|v| v.checked_add(digit))
                .ok_or(BencodeError::IntegerOverflow)?
        };
        digits += 1;
        *pos += 1;
    }
}

fn decode_bytes(input: &[u8], pos: &mut usize) -> Result<Bytes, BencodeError> {
    let first = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
    if !first.is_ascii_digit() {
        return Err(BencodeError::InvalidSyntax);
    }
    let mut len = 0usize;
    let mut digits = 0usize;
    loop {
        let byte = *input.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
        if byte == b':' {
            *pos += 1;
            break;
        }
        if !byte.is_ascii_digit() {
            return Err(BencodeError::InvalidSyntax);
        }
        if digits == 1 && first == b'0' {
            return Err(BencodeError::InvalidSyntax);
        }
        len = len
            .checked_mul(10)
            .and_then(|v| v.checked_add(usize::from(byte - b'0')))
            .ok_or(BencodeError::IntegerOverflow)?;
        digits += 1;
        *pos += 1;
    }
    let end = pos.checked_add(len).ok_or(BencodeError::IntegerOverflow)?;
    if end > input.len() {
        return Err(BencodeError::UnexpectedEof);
    }
    let slice = Bytes::copy_from_slice(&input[*pos..end]);
    *pos = end;
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_prefix_separates_header_dict_from_binary_piece_bytes() {
        let payload = b"d8:msg_typei1e5:piecei0e10:total_sizei4ee\xff\x00\xab\xcd";
        let (val, consumed) = decode_prefix(payload).unwrap();
        let dict = val.as_dict().unwrap();
        assert_eq!(dict.get(&b"msg_type"[..]).unwrap().as_integer(), Some(1));
        assert_eq!(dict.get(&b"piece"[..]).unwrap().as_integer(), Some(0));
        assert_eq!(dict.get(&b"total_size"[..]).unwrap().as_integer(), Some(4));
        assert_eq!(&payload[consumed..], b"\xff\x00\xab\xcd");
        assert_eq!(decode_exact(payload), Err(BencodeError::TrailingBytes));
    }

    #[test]
    fn round_trip_and_integer_edge_cases() {
        let mut dict = BTreeMap::new();
        dict.insert(
            Bytes::from_static(b"min"),
            BencodeValue::Integer(i64::MIN),
        );
        dict.insert(
            Bytes::from_static(b"max"),
            BencodeValue::Integer(i64::MAX),
        );
        dict.insert(Bytes::from_static(b"zero"), BencodeValue::Integer(0));
        dict.insert(
            Bytes::from_static(b"list"),
            BencodeValue::List(vec![
                BencodeValue::string("abc"),
                BencodeValue::Bytes(Bytes::new()),
            ]),
        );
        let original = BencodeValue::Dict(dict);
        let bytes = encode(&original);
        assert_eq!(decode_exact(&bytes).unwrap(), original);

        assert_eq!(decode_exact(b"i-0e"), Err(BencodeError::InvalidSyntax));
        assert_eq!(decode_exact(b"i00e"), Err(BencodeError::InvalidSyntax));
        assert_eq!(
            decode_exact(b"i9223372036854775808e"),
            Err(BencodeError::IntegerOverflow)
        );
        assert_eq!(decode_exact(b"01:a"), Err(BencodeError::InvalidSyntax));
        assert_eq!(decode_exact(b"5:ab"), Err(BencodeError::UnexpectedEof));
    }

    #[test]
    fn excessive_nesting_depth_is_rejected() {
        let mut ok_nested = vec![b'l'; MAX_BENCODE_DEPTH];
        ok_nested.extend_from_slice(b"i1e");
        ok_nested.extend(std::iter::repeat_n(b'e', MAX_BENCODE_DEPTH));
        assert!(decode_exact(&ok_nested).is_ok());

        let mut too_deep = vec![b'l'; MAX_BENCODE_DEPTH + 1];
        too_deep.extend_from_slice(b"i1e");
        too_deep.extend(std::iter::repeat_n(b'e', MAX_BENCODE_DEPTH + 1));
        assert_eq!(decode_exact(&too_deep), Err(BencodeError::DepthLimit));
    }
}
