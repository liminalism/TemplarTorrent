use std::collections::BTreeMap;

use bytes::Bytes;

use super::error::BencodeError;
use super::value::Value;

const MAX_DEPTH: usize = 64;

/// Decodes a bencode value from a byte slice.
///
/// The input must contain exactly one complete bencode value with no trailing data.
///
/// # Errors
///
/// Returns an error if:
/// - The input is empty or truncated ([`BencodeError::UnexpectedEof`])
/// - The input contains invalid bencode syntax
/// - The nesting depth exceeds 64 levels ([`BencodeError::NestingTooDeep`])
/// - There is data after the value ([`BencodeError::TrailingData`])
///
/// # Examples
///
/// ```
/// use rbit::bencode::decode;
///
/// // Decode different types
/// let int = decode(b"i42e").unwrap();
/// assert_eq!(int.as_integer(), Some(42));
///
/// let string = decode(b"5:hello").unwrap();
/// assert_eq!(string.as_str(), Some("hello"));
///
/// let list = decode(b"li1ei2ei3ee").unwrap();
/// assert_eq!(list.as_list().unwrap().len(), 3);
///
/// let dict = decode(b"d4:name5:Alice3:agei30ee").unwrap();
/// assert_eq!(dict.get(b"name").and_then(|v| v.as_str()), Some("Alice"));
/// ```
pub fn decode(data: &[u8]) -> Result<Value, BencodeError> {
    let mut pos = 0;
    let value = decode_value(data, &mut pos, 0)?;

    if pos != data.len() {
        return Err(BencodeError::TrailingData);
    }

    Ok(value)
}

fn decode_value(data: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    if depth > MAX_DEPTH {
        return Err(BencodeError::NestingTooDeep);
    }

    if *pos >= data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    match data[*pos] {
        b'i' => decode_integer(data, pos),
        b'l' => decode_list(data, pos, depth),
        b'd' => decode_dict(data, pos, depth),
        b'0'..=b'9' => decode_bytes(data, pos),
        c => Err(BencodeError::UnexpectedChar(c as char)),
    }
}

fn decode_integer(data: &[u8], pos: &mut usize) -> Result<Value, BencodeError> {
    *pos += 1;

    let start = *pos;
    while *pos < data.len() && data[*pos] != b'e' {
        *pos += 1;
    }

    if *pos >= data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    let int_str = std::str::from_utf8(&data[start..*pos])
        .map_err(|_| BencodeError::InvalidInteger("invalid utf8".into()))?;

    if int_str.is_empty() {
        return Err(BencodeError::InvalidInteger("empty".into()));
    }

    if int_str.starts_with("-0") || (int_str.starts_with('0') && int_str.len() > 1) {
        return Err(BencodeError::InvalidInteger("leading zeros".into()));
    }

    let value: i64 = int_str
        .parse()
        .map_err(|_| BencodeError::InvalidInteger(int_str.into()))?;

    *pos += 1;
    Ok(Value::Integer(value))
}

fn decode_bytes(data: &[u8], pos: &mut usize) -> Result<Value, BencodeError> {
    let start = *pos;
    while *pos < data.len() && data[*pos] != b':' {
        *pos += 1;
    }

    if *pos >= data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    let len_str =
        std::str::from_utf8(&data[start..*pos]).map_err(|_| BencodeError::InvalidStringLength)?;

    let len: usize = len_str
        .parse()
        .map_err(|_| BencodeError::InvalidStringLength)?;

    *pos += 1;

    if *pos + len > data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    let bytes = Bytes::copy_from_slice(&data[*pos..*pos + len]);
    *pos += len;

    Ok(Value::Bytes(bytes))
}

fn decode_list(data: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    *pos += 1;
    let mut list = Vec::new();

    while *pos < data.len() && data[*pos] != b'e' {
        list.push(decode_value(data, pos, depth + 1)?);
    }

    if *pos >= data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    *pos += 1;
    Ok(Value::List(list))
}

fn decode_dict(data: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    *pos += 1;
    let mut dict = BTreeMap::new();

    while *pos < data.len() && data[*pos] != b'e' {
        let key = match decode_value(data, pos, depth + 1)? {
            Value::Bytes(b) => b,
            _ => return Err(BencodeError::UnexpectedChar(data[*pos] as char)),
        };

        let value = decode_value(data, pos, depth + 1)?;
        dict.insert(key, value);
    }

    if *pos >= data.len() {
        return Err(BencodeError::UnexpectedEof);
    }

    *pos += 1;
    Ok(Value::Dict(dict))
}
