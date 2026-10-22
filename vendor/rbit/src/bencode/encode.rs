use std::io::Write;

use super::error::BencodeError;
use super::value::Value;

/// Encodes a bencode value to a byte vector.
///
/// The output follows the canonical bencode format:
/// - Integers: `i<number>e`
/// - Byte strings: `<length>:<data>`
/// - Lists: `l<items>e`
/// - Dictionaries: `d<key><value>...e` (keys sorted lexicographically)
///
/// # Errors
///
/// Returns an error if writing to the internal buffer fails.
///
/// # Examples
///
/// ```
/// use rbit::bencode::{encode, Value};
/// use std::collections::BTreeMap;
/// use bytes::Bytes;
///
/// // Encode an integer
/// let encoded = encode(&Value::Integer(42)).unwrap();
/// assert_eq!(encoded, b"i42e");
///
/// // Encode a string
/// let encoded = encode(&Value::string("hello")).unwrap();
/// assert_eq!(encoded, b"5:hello");
///
/// // Encode a list
/// let list = Value::List(vec![Value::Integer(1), Value::string("two")]);
/// let encoded = encode(&list).unwrap();
/// assert_eq!(encoded, b"li1e3:twoe");
///
/// // Encode a dictionary
/// let mut dict = BTreeMap::new();
/// dict.insert(Bytes::from_static(b"a"), Value::Integer(1));
/// dict.insert(Bytes::from_static(b"b"), Value::Integer(2));
/// let encoded = encode(&Value::Dict(dict)).unwrap();
/// assert_eq!(encoded, b"d1:ai1e1:bi2ee");
/// ```
pub fn encode(value: &Value) -> Result<Vec<u8>, BencodeError> {
    let mut buf = Vec::new();
    encode_value(value, &mut buf)?;
    Ok(buf)
}

fn encode_value<W: Write>(value: &Value, writer: &mut W) -> Result<(), BencodeError> {
    match value {
        Value::Integer(i) => {
            write!(writer, "i{}e", i)?;
        }
        Value::Bytes(b) => {
            write!(writer, "{}:", b.len())?;
            writer.write_all(b)?;
        }
        Value::List(l) => {
            writer.write_all(b"l")?;
            for item in l {
                encode_value(item, writer)?;
            }
            writer.write_all(b"e")?;
        }
        Value::Dict(d) => {
            writer.write_all(b"d")?;
            for (key, val) in d {
                write!(writer, "{}:", key.len())?;
                writer.write_all(key)?;
                encode_value(val, writer)?;
            }
            writer.write_all(b"e")?;
        }
    }
    Ok(())
}
